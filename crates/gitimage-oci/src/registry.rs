//! `Storage` that proxies to an upstream OCI registry (registry mode).
//! TEMPORARY STUB on main; replaced by the feat/oci implementation.
pub struct RegistryStorage;

impl RegistryStorage {
    pub fn from_dockerconfigjson(
        _url: &str,
        _json: &[u8],
        _insecure: bool,
    ) -> Result<Self, crate::OciError> {
        Err(crate::OciError::Unsupported)
    }
}
