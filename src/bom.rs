// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Data bill of materials: a CycloneDX 1.7 document saying which version
//! of osmsignals made an output file from which input. It is written
//! next to the output, with the same name and `.cdx.json` instead of
//! `.tiff`.
//!
//! The input is streamed and only partly read, so we cannot hash it;
//! its component identifies it by name, replication timestamp and URL
//! instead. Every timestamp comes from the input, not the clock, and
//! the serial number is derived from the output's hash, so rebuilding
//! from the same planet with the same release gives the same document.

use anyhow::{Context, Result};
use ring::digest;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};

const REPO_URL: &str = "https://github.com/brawer/osmsignals";
const ODBL_URL: &str = "https://opendatacommons.org/licenses/odbl/1-0/";
const OSM_COPYRIGHT: &str = "© OpenStreetMap contributors";

/// CycloneDX allows no media type parameters, so this cannot say
/// `application=geotiff; profile=cloud-optimized` as the OGC does.
const COG_MIME_TYPE: &str = "image/tiff";

/// Namespace for the version-5 UUID in `serialNumber`, which is
/// UUIDv5(URL, "https://github.com/brawer/osmsignals#data-bom-serial").
/// Frozen: changing it would change every serial number.
const SERIAL_NAMESPACE: [u8; 16] = [
    0x3b, 0xfe, 0xa3, 0xc0, 0xa1, 0x58, 0x5d, 0xcd, 0x82, 0x43, 0xee, 0xbe, 0x86, 0xde, 0xdc, 0x1a,
];

/// Where the input came from.
#[derive(Clone, Debug, Default)]
pub struct Input {
    /// File name, such as `planet-261012.osm.pbf`.
    pub name: String,
    /// Where it was downloaded from, after redirects.
    pub url: Option<String>,
}

impl Input {
    /// Describes `input` as given on the command line: a path, a URL
    /// (pass the URL after redirects), or "-" for standard input.
    pub fn new(input: &str) -> Input {
        if input.starts_with("https://") || input.starts_with("http://") {
            let path = input.split(['?', '#']).next().unwrap_or(input);
            let name = path.rsplit('/').next().unwrap_or(path);
            return Input {
                name: name.to_string(),
                url: Some(input.to_string()),
            };
        }
        if input == "-" {
            return Input {
                name: "stdin".into(),
                url: None,
            };
        }
        let name = Path::new(input)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "stdin".into());
        Input { name, url: None }
    }
}

/// What a signal pipeline tells the BOM about its run.
pub struct Run<'a> {
    pub input: &'a Input,
    /// Replication timestamp of the input, in seconds since the epoch.
    pub timestamp: i64,
    pub output: &'a Path,
    pub description: &'a str,
    /// Identifies this run in its scheduler, such as a Kubernetes job;
    /// empty if not known.
    pub run_id: &'a str,
}

/// Path of the BOM for `output`.
pub fn bom_path(output: &Path) -> PathBuf {
    output.with_extension("cdx.json")
}

/// Writes the BOM for a finished output file with the given hashes,
/// and returns its path.
pub fn write(run: &Run, output: &Hashes) -> Result<PathBuf> {
    let bom = build(run, &output.sha256, &output.sha512);
    let path = bom_path(run.output);
    let mut text = serde_json::to_string_pretty(&bom)?;
    text.push('\n');
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, text).with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

fn build(run: &Run, sha256: &str, sha512: &str) -> Value {
    let version = env!("CARGO_PKG_VERSION");
    let anchor = rfc3339(run.timestamp);
    let output_name = run
        .output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let input_ref = run.input.name.clone();

    // Planet dumps share one package name, whatever their file name.
    let package = if run.input.name.starts_with("planet-") {
        "planet"
    } else {
        run.input.name.as_str()
    };
    let mut purl = format!("pkg:generic/openstreetmap/{package}@{anchor}");
    let mut input_refs = vec![json!({"type": "license", "url": ODBL_URL})];
    let mut input_data = json!({"type": "dataset"});
    if let Some(url) = &run.input.url {
        purl.push_str(&format!("?download_url={url}"));
        input_refs.insert(0, json!({"type": "distribution", "url": url}));
        input_data["contents"] = json!({"url": url});
    }

    json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.7",
        "serialNumber": serial_number(sha256),
        "version": 1,
        "metadata": {
            "timestamp": anchor,
            "supplier": supplier(),
            "tools": {
                "components": [{
                    "bom-ref": "tool-osmsignals",
                    "type": "application",
                    "name": "osmsignals",
                    "version": version,
                    "purl": format!("pkg:github/brawer/osmsignals@v{version}"),
                    "externalReferences": [
                        {"type": "vcs", "url": REPO_URL},
                        {"type": "release-notes", "url": format!("{REPO_URL}/releases/tag/v{version}")},
                    ],
                }],
            },
            "component": {
                "bom-ref": output_name,
                "type": "data",
                "name": output_name,
                "mime-type": COG_MIME_TYPE,
                "version": anchor,
                "description": run.description,
                "supplier": supplier(),
                "licenses": license(),
                "copyright": OSM_COPYRIGHT,
                "hashes": [
                    {"alg": "SHA-256", "content": sha256},
                    {"alg": "SHA-512", "content": sha512},
                ],
                "data": [{"type": "dataset"}],
                "externalReferences": [
                    {"type": "documentation", "url": format!("{REPO_URL}#readme")},
                    {"type": "license", "url": ODBL_URL},
                ],
            },
        },
        "components": [{
            "bom-ref": input_ref,
            "type": "data",
            "name": run.input.name,
            "version": anchor,
            "supplier": {
                "name": "OpenStreetMap Foundation",
                "url": ["https://osmfoundation.org/"],
            },
            "licenses": license(),
            "copyright": OSM_COPYRIGHT,
            "purl": purl,
            "data": [input_data],
            "externalReferences": input_refs,
        }],
        "dependencies": [{"ref": output_name, "dependsOn": [input_ref]}],
        "formulation": [{
            "bom-ref": "formula-osmsignals",
            "workflows": [{
                "bom-ref": "workflow-osmsignals",
                "uid": run.run_id,
                "name": format!("osmsignals {}", run.description),
                "taskTypes": ["build"],
                "timeStart": anchor,
                "timeEnd": anchor,
                "resourceReferences": [{"ref": "tool-osmsignals"}],
                "inputs": [{"resource": {"ref": input_ref}}],
                "outputs": [{"resource": {"ref": output_name}}],
            }],
        }],
    })
}

fn supplier() -> Value {
    json!({"name": "Sascha Brawer", "url": ["https://brawer.ch"]})
}

fn license() -> Value {
    json!([{"license": {"id": "ODbL-1.0", "url": ODBL_URL, "acknowledgement": "declared"}}])
}

/// Size and digests of a file, as lowercase hex.
#[derive(Clone, Debug)]
pub struct Hashes {
    pub bytes: u64,
    pub sha256: String,
    pub sha512: String,
}

pub fn hash_file(path: &Path) -> Result<Hashes> {
    let mut f = std::fs::File::open(path).with_context(|| format!("hashing {}", path.display()))?;
    let mut bytes = 0;
    let mut sha256 = digest::Context::new(&digest::SHA256);
    let mut sha512 = digest::Context::new(&digest::SHA512);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha256.update(&buf[..n]);
        sha512.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok(Hashes {
        bytes,
        sha256: hex(sha256.finish().as_ref()),
        sha512: hex(sha512.finish().as_ref()),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Version-5 UUID of `name` in [`SERIAL_NAMESPACE`], as a URN.
fn serial_number(name: &str) -> String {
    uuid_v5(&SERIAL_NAMESPACE, name.as_bytes())
}

pub(crate) fn uuid_v5(namespace: &[u8; 16], name: &[u8]) -> String {
    let mut ctx = digest::Context::new(&digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(namespace);
    ctx.update(name);
    let mut b = [0u8; 16];
    b.copy_from_slice(&ctx.finish().as_ref()[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// RFC 3339 form of `timestamp`, in UTC.
pub(crate) fn rfc3339(timestamp: i64) -> String {
    let (y, m, d) = crate::node_density::civil_date(timestamp.div_euclid(86_400));
    let s = timestamp.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v5_matches_rfc_9562() {
        // RFC 9562, appendix A.4: UUIDv5 of "www.example.com" in the DNS namespace.
        let dns = [
            0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4,
            0x30, 0xc8,
        ];
        assert_eq!(
            uuid_v5(&dns, b"www.example.com"),
            "urn:uuid:2ed6657d-e927-568b-95e1-2665a8aea6a2"
        );
    }

    #[test]
    fn serial_namespace_is_derived_as_documented() {
        let url_ns = [
            0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4,
            0x30, 0xc8,
        ];
        let derived = uuid_v5(
            &url_ns,
            b"https://github.com/brawer/osmsignals#data-bom-serial",
        );
        assert_eq!(derived, "urn:uuid:3bfea3c0-a158-5dcd-8243-eebe86dedc1a");
        assert_eq!(
            uuid_v5(
                &url_ns,
                b"https://github.com/brawer/osmsignals#osm-node-density"
            ),
            "urn:uuid:2c7bffea-cc30-533e-ac1d-dc7b4aefd158",
            "data package id of node-density"
        );
        assert_eq!(
            derived.replace('-', "")["urn:uuid:".len()..],
            hex(&SERIAL_NAMESPACE)
        );
    }

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_769_501_462), "2026-01-27T08:11:02Z");
    }

    #[test]
    fn input_names() {
        let url = "https://example.org/pbf/2026/planet-261012.osm.pbf?x=1";
        let i = Input::new(url);
        assert_eq!(i.name, "planet-261012.osm.pbf");
        assert_eq!(i.url.as_deref(), Some(url));
        let i = Input::new("/data/zugerland.osm.pbf");
        assert_eq!(i.name, "zugerland.osm.pbf");
        assert!(i.url.is_none());
        assert_eq!(Input::new("-").name, "stdin");
    }

    #[test]
    fn bom_path_replaces_extension() {
        assert_eq!(
            bom_path(Path::new("out/osm-node-density.2026-10-12.tiff")),
            Path::new("out/osm-node-density.2026-10-12.cdx.json")
        );
    }
}
