//! Format-preserving edits to the config file.
//!
//! The settings UI writes through this rather than serialising a
//! `Config` back out. Serde round-tripping would produce a valid file
//! and silently delete every comment in it — and this config is mostly
//! comments (see the sample in the README). `toml_edit` patches
//! individual keys and leaves the rest of the document, including
//! spacing and comments, exactly as the user wrote it.
//!
//! Saving is atomic: write a sibling temp file, then rename over the
//! target. A crash mid-write can't leave a half-written config, and the
//! rename is what [`crate::config_watch`] sees — it polls the path, so
//! replace-by-rename is picked up correctly.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, value};

pub struct ConfigFile {
    path: PathBuf,
    doc: DocumentMut,
}

impl ConfigFile {
    /// Load the document for editing. A missing file is not an error —
    /// it yields an empty document, so the first setting written
    /// creates the file with just that key in it.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let doc: DocumentMut = text
            .parse()
            .with_context(|| format!("parse {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            doc,
        })
    }

    pub fn set_f64(&mut self, table_path: &[&str], key: &str, v: f64) -> Result<()> {
        self.table_mut(table_path)?[key] = value(v);
        Ok(())
    }

    pub fn set_bool(&mut self, table_path: &[&str], key: &str, v: bool) -> Result<()> {
        self.table_mut(table_path)?[key] = value(v);
        Ok(())
    }

    pub fn set_str(&mut self, table_path: &[&str], key: &str, v: &str) -> Result<()> {
        self.table_mut(table_path)?[key] = value(v);
        Ok(())
    }

    /// Walk to a table, creating any missing levels. Intermediate
    /// tables are marked implicit so creating `[gestures.swipe.vertical]`
    /// doesn't also emit empty `[gestures]` and `[gestures.swipe]`
    /// headers.
    fn table_mut(&mut self, path: &[&str]) -> Result<&mut Table> {
        let mut item: &mut Item = self.doc.as_item_mut();
        for (i, segment) in path.iter().enumerate() {
            let table = item
                .as_table_mut()
                .with_context(|| format!("config path {:?} is not a table", &path[..i]))?;
            if !table.contains_key(segment) {
                let mut fresh = Table::new();
                fresh.set_implicit(i + 1 < path.len());
                table.insert(segment, Item::Table(fresh));
            }
            item = table
                .get_mut(segment)
                .expect("just inserted if it was missing");
        }
        item.as_table_mut()
            .with_context(|| format!("config path {path:?} is not a table"))
    }

    /// Write the document back, atomically.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        // Sibling temp file: rename is only atomic within a filesystem.
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.doc.to_string())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename into {}", self.path.display()))?;
        Ok(())
    }

    #[cfg(test)]
    fn to_text(&self) -> String {
        self.doc.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_text(text: &str) -> ConfigFile {
        ConfigFile {
            path: PathBuf::from("/dev/null/unused"),
            doc: text.parse().expect("test input parses"),
        }
    }

    #[test]
    fn changing_a_value_keeps_comments_and_layout() {
        let mut f = from_text(
            "# tuning notes worth keeping\n\
             [cursor]\n\
             # px per mm at accel_ref\n\
             sensitivity = 25.0\n\
             accel_exponent = 1.0  # inline comment\n",
        );
        f.set_f64(&["cursor"], "sensitivity", 44.0).unwrap();
        let out = f.to_text();

        assert!(out.contains("# tuning notes worth keeping"));
        assert!(out.contains("# px per mm at accel_ref"));
        assert!(out.contains("# inline comment"));
        assert!(out.contains("sensitivity = 44.0"));
        assert!(!out.contains("25.0"));
    }

    #[test]
    fn adding_a_key_leaves_other_tables_untouched() {
        let mut f =
            from_text("[cursor]\nsensitivity = 25.0\n\n# scrolling\n[scroll]\nnatural = true\n");
        f.set_bool(&["scroll"], "natural", false).unwrap();
        f.set_f64(&["scroll"], "sensitivity", 33.0).unwrap();
        let out = f.to_text();

        assert!(out.contains("# scrolling"));
        assert!(out.contains("sensitivity = 25.0"), "cursor table survived");
        assert!(out.contains("natural = false"));
        assert!(out.contains("sensitivity = 33.0"));
    }

    #[test]
    fn creates_nested_tables_without_emitting_empty_parents() {
        let mut f = from_text("[cursor]\nsensitivity = 25.0\n");
        f.set_str(&["gestures", "swipe", "vertical"], "backend", "off")
            .unwrap();
        let out = f.to_text();

        assert!(out.contains("[gestures.swipe.vertical]"));
        assert!(out.contains("backend = \"off\""));
        // The implicit parents must not appear as empty headers.
        assert!(!out.contains("[gestures]\n"), "no empty [gestures]:\n{out}");
        assert!(
            !out.contains("[gestures.swipe]\n"),
            "no empty parent:\n{out}"
        );
    }

    #[test]
    fn writing_into_an_empty_document_creates_the_table() {
        let mut f = from_text("");
        f.set_f64(&["cursor"], "sensitivity", 30.0).unwrap();
        let out = f.to_text();
        assert!(out.contains("[cursor]"));
        assert!(out.contains("sensitivity = 30.0"));
    }

    #[test]
    fn a_scalar_where_a_table_belongs_is_an_error_not_a_panic() {
        let mut f = from_text("cursor = 3\n");
        assert!(f.set_f64(&["cursor"], "sensitivity", 1.0).is_err());
    }

    #[test]
    fn save_then_reload_round_trips() {
        let dir = std::env::temp_dir().join(format!("tpc-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# keep me\n[cursor]\nsensitivity = 25.0\n").unwrap();

        let mut f = ConfigFile::load(&path).unwrap();
        f.set_f64(&["cursor"], "sensitivity", 51.0).unwrap();
        f.save().unwrap();

        let back = std::fs::read_to_string(&path).unwrap();
        assert!(back.contains("# keep me"));
        assert!(back.contains("sensitivity = 51.0"));
        // The temp file must not be left behind.
        assert!(!path.with_extension("toml.tmp").exists());

        // And the result is still valid for the strict loader.
        let parsed: crate::config::Config = toml::from_str(&back).unwrap();
        assert_eq!(parsed.cursor.sensitivity, 51.0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
