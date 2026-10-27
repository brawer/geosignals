// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! The `feeds/*.json` files of a catalog repository, such as
//! Transitous or the Transitland Atlas, read from a checkout or from
//! the tarball that GitHub serves for a branch.
//!
//! GitHub makes those tarballs with `git archive`, which puts the
//! commit id into a pax global header; we keep it for the data BOM.

use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

/// URL of the tarball of a GitHub repository's branch.
pub fn github_tarball(repo: &str, branch: &str) -> String {
    format!("https://github.com/{repo}/archive/refs/heads/{branch}.tar.gz")
}

#[derive(Debug, Default, PartialEq)]
pub struct FeedFiles {
    /// File stem (such as `ch` for `feeds/ch.json`) and content, sorted.
    pub files: Vec<(String, Vec<u8>)>,
    /// The commit the files come from, if known.
    pub commit: Option<String>,
}

/// Reads `feeds/*.json` of a repository checkout at `dir`.
pub fn from_dir(dir: &Path) -> Result<FeedFiles> {
    let feeds = dir.join("feeds");
    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(&feeds).with_context(|| format!("reading {}", feeds.display()))?
    {
        let path = entry?.path();
        if let Some(stem) = json_stem(&path.to_string_lossy()) {
            let data =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            files.push((stem.to_string(), data));
        }
    }
    files.sort();
    Ok(FeedFiles {
        files,
        commit: None,
    })
}

/// Reads `<top>/feeds/*.json` from a gzipped tarball of a repository.
pub fn from_tar_gz(r: impl Read) -> Result<FeedFiles> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(r));
    let mut out = FeedFiles::default();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type() == tar::EntryType::XGlobalHeader {
            if let Some(exts) = entry.pax_extensions()? {
                for ext in exts {
                    let ext = ext?;
                    if ext.key() == Ok("comment")
                        && let Ok(v) = ext.value()
                        && v.len() == 40
                        && v.bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        out.commit = Some(v.to_string());
                    }
                }
            }
            continue;
        }
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().into_owned();
        let mut parts = path.split('/');
        let (Some(_top), Some("feeds"), Some(name), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let Some(stem) = json_stem(name) {
            let stem = stem.to_string();
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            out.files.push((stem, data));
        }
    }
    out.files.sort();
    Ok(out)
}

/// `ch` for `.../ch.json`.
fn json_stem(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    name.strip_suffix(".json").filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tarball like GitHub's: pax global header with the commit, a
    /// top directory, feeds and other files.
    pub fn tarball(commit: &str, files: &[(&str, &str)]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        let pax = format!("comment={commit}\n");
        let record = format!("{} {pax}", pax.len() + 3);
        let mut h = tar::Header::new_ustar();
        h.set_path("pax_global_header").unwrap();
        h.set_entry_type(tar::EntryType::XGlobalHeader);
        h.set_size(record.len() as u64);
        h.set_cksum();
        b.append(&h, record.as_bytes()).unwrap();
        for (path, content) in files {
            let mut h = tar::Header::new_ustar();
            h.set_path(path).unwrap();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, content.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn reads_github_tarball() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let data = tarball(
            commit,
            &[
                ("transitous-main/README.md", "hi"),
                ("transitous-main/feeds/de.json", "{\"sources\": []}"),
                ("transitous-main/feeds/ch.json", "{}"),
                ("transitous-main/feeds/sub/x.json", "{}"),
                ("transitous-main/feeds/notes.txt", ""),
            ],
        );
        let f = from_tar_gz(data.as_slice()).unwrap();
        assert_eq!(f.commit.as_deref(), Some(commit));
        let names: Vec<&str> = f.files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["ch", "de"]);
        assert_eq!(f.files[1].1, b"{\"sources\": []}");
        assert!(from_tar_gz(&b"not gzip"[..]).is_err());
    }

    #[test]
    fn reads_checkout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("feeds")).unwrap();
        std::fs::write(dir.path().join("feeds/ch.json"), "{}").unwrap();
        std::fs::write(dir.path().join("feeds/README.md"), "").unwrap();
        let f = from_dir(dir.path()).unwrap();
        assert_eq!(f.files, [("ch".to_string(), b"{}".to_vec())]);
        assert_eq!(f.commit, None);
        use crate::transit::*;
        assert_eq!(github_tarball(TRANSITOUS, "main"), TRANSITOUS_URL);
        assert_eq!(
            github_tarball(TRANSITLAND_ATLAS, "main"),
            TRANSITLAND_ATLAS_URL
        );
    }
}
