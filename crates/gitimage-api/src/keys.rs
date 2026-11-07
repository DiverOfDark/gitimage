//! Annotation, label and scheduling-gate keys. Every component reads these constants;
//! none of them spell the strings themselves.

/// Prefix of the pod-template annotation `gitimage.dev/image.<container>: <gitimage>`.
pub const IMAGE_ANNOTATION_PREFIX: &str = "gitimage.dev/image.";
/// Prefix of the pod-template annotation `gitimage.dev/revision.<container>: <sha>`,
/// written by the workload roller with server-side apply.
pub const REVISION_ANNOTATION_PREFIX: &str = "gitimage.dev/revision.";
/// `gitimage.dev/rollout: manual` on a workload opts it out of automatic rollouts.
pub const ROLLOUT_ANNOTATION: &str = "gitimage.dev/rollout";
/// Any change of `gitimage.dev/rebuild` on a GitImage retries a failed build of the same sha.
pub const REBUILD_ANNOTATION: &str = "gitimage.dev/rebuild";
/// Written by the hooks receiver on every replica; the leader turns it into a reconcile.
pub const HOOK_AT_ANNOTATION: &str = "gitimage.dev/hook-at";

/// Label the webhook puts on gated pods so the gate releaser can watch with a selector.
pub const GATED_LABEL: &str = "gitimage.dev/gated";
/// Label on build Jobs and their pods.
pub const ROLE_LABEL: &str = "gitimage.dev/role";
pub const ROLE_BUILD: &str = "build";
/// Label carrying the owning GitImage's name on build Jobs.
pub const NAME_LABEL: &str = "gitimage.dev/name";
/// Label carrying the commit sha on build Jobs.
pub const REVISION_LABEL: &str = "gitimage.dev/revision";

/// Scheduling gate held until every annotated container has a digest.
pub const SCHEDULING_GATE: &str = "gitimage.dev/build";

/// Field manager for every server-side apply the controller does.
pub const FIELD_MANAGER: &str = "gitimage-controller";

/// Exit code of `gitimage clone` when the context tree is unchanged (F7 fallback).
pub const CLONE_EXIT_UNCHANGED: i32 = 3;

/// Returns the container name if `key` is a `gitimage.dev/image.<container>` annotation.
pub fn image_annotation_container(key: &str) -> Option<&str> {
    key.strip_prefix(IMAGE_ANNOTATION_PREFIX)
        .filter(|c| !c.is_empty())
}

/// `gitimage.dev/revision.<container>`
pub fn revision_annotation(container: &str) -> String {
    format!("{REVISION_ANNOTATION_PREFIX}{container}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_image_annotation() {
        assert_eq!(
            image_annotation_container("gitimage.dev/image.web"),
            Some("web")
        );
        assert_eq!(image_annotation_container("gitimage.dev/image."), None);
        assert_eq!(
            image_annotation_container("gitimage.dev/revision.web"),
            None
        );
    }

    #[test]
    fn builds_revision_annotation() {
        assert_eq!(revision_annotation("web"), "gitimage.dev/revision.web");
    }
}
