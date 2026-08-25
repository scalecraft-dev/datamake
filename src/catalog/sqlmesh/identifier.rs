//! SQLMesh's snapshot identifier (ADR 0016 §3): the `_snapshots` primary
//! key is `(name, identifier)`, and `(name, version)` is **not** unique —
//! mountain's prod join fans out 3.2× on it. The environment row carries
//! only the fingerprint, so the identifier is recomputed exactly as SQLMesh
//! does (`SnapshotFingerprint.to_identifier`, `utils/hashing.py`):
//! `str(zlib.crc32(";".join(data_hash, metadata_hash, parent_data_hash,
//! parent_metadata_hash)))` — **decimal**, not hex.

/// The four hashes of a snapshot fingerprint, as strings, in SQLMesh's
/// order. A missing hash (`None`) joins as the empty string, as SQLMesh's
/// `_safe_concat` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub data_hash: String,
    pub metadata_hash: String,
    pub parent_data_hash: String,
    pub parent_metadata_hash: String,
}

impl Fingerprint {
    pub fn identifier(&self) -> String {
        let joined = [
            self.data_hash.as_str(),
            self.metadata_hash.as_str(),
            self.parent_data_hash.as_str(),
            self.parent_metadata_hash.as_str(),
        ]
        .join(";");
        crc32fast::hash(joined.as_bytes()).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live values from the fixture state DB and from mountain's census
    /// (`test/fixtures/sqlmesh/mountain_flight_spend_snapshot.json`).
    #[test]
    fn reproduces_sqlmesh_identifiers_in_decimal() {
        let fp = Fingerprint {
            data_hash: "3860973680".into(),
            metadata_hash: "2481114527".into(),
            parent_data_hash: "0".into(),
            parent_metadata_hash: "0".into(),
        };
        assert_eq!(fp.identifier(), "1925392354");

        let fp = Fingerprint {
            data_hash: "2857378421".into(),
            metadata_hash: "3972671151".into(),
            parent_data_hash: "2113935511".into(),
            parent_metadata_hash: "1626807955".into(),
        };
        assert_eq!(fp.identifier(), "3237974788");
    }
}
