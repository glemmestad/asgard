//! Pure, storage- and network-free skill primitives shared across Frontkeep: the
//! bundle/manifest model, a `SKILL.md` frontmatter parser, the Claude Code ⇄ Codex
//! translation layer, and a portability lint. Everything here is deterministic and
//! unit-testable; persistence lives in `frontkeep-registry`, wiring in `frontkeep-api` /
//! `frontkeep-mcp`.

mod dest;
mod model;
mod translate;

pub use dest::{destination, slug, Destination, DESTINATIONS};
pub use model::{
    content_bytes, from_json, frontmatter_json, parse_manifest, safe_path, split_frontmatter,
    store, validate, Runtime, SkillBundle, SkillError, SkillFile, SkillManifest, Stored,
    MAX_BUNDLE_BYTES, MAX_FILES, MAX_FILE_BYTES, RUNTIMES, SKILL_FILE,
};
pub use translate::{
    lint_portability, translate, Action, CompatReport, FieldRule, LossEntry, Portability,
    TranslationResult, CLAUDE_TO_CODEX,
};
