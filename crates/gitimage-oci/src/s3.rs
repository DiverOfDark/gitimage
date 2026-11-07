//! `Storage` on S3-compatible object storage via object_store (s3 mode).
//! TEMPORARY STUB on main; replaced by the feat/oci implementation.
pub struct S3Storage;

impl S3Storage {
    pub fn new(
        _cfg: &gitimage_api::config::S3Storage,
        _access_key: String,
        _secret_key: String,
    ) -> Result<Self, crate::OciError> {
        Err(crate::OciError::Unsupported)
    }
}
