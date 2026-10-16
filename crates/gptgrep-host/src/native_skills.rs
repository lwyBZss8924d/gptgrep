//! Explicit native Skill inputs over caller-selected, pinned local files.
//! Listing and input submission do not independently prove body consumption.
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

const MAX_SKILLS: usize = 64;
const MAX_SKILL_BYTES: u64 = 1_048_576;

/// Caller-selected native Skill, including the exact local source digest.
/// Paths and observations are private runtime metadata, not public projections.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeSkillInput {
    pub name: String,
    pub path: PathBuf,
    pub sha256: String,
}

pub(crate) fn validate(skills: &[NativeSkillInput]) -> Result<()> {
    ensure!(skills.len() <= MAX_SKILLS, "host_native_skills_limit");
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for skill in skills {
        ensure!(
            !skill.name.trim().is_empty()
                && skill.name.len() <= 256
                && !skill.name.chars().any(char::is_control)
                && names.insert(skill.name.as_str())
                && paths.insert(skill.path.as_path()),
            "host_native_skill_identity"
        );
        ensure!(
            skill.sha256.len() == 64
                && skill
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "host_native_skill_digest"
        );
        ensure!(
            skill.path.is_absolute()
                && skill.path.file_name() == Some(OsStr::new("SKILL.md"))
                && skill
                    .path
                    .to_str()
                    .is_some_and(|path| path.len() <= 4096 && !path.chars().any(char::is_control)),
            "host_native_skill_path"
        );
        verify_file(skill)?;
    }
    Ok(())
}

fn verify_file(skill: &NativeSkillInput) -> Result<()> {
    ensure!(
        fs::canonicalize(&skill.path).map_err(|_| anyhow!("host_native_skill_path"))? == skill.path,
        "host_native_skill_path"
    );
    let before =
        fs::symlink_metadata(&skill.path).map_err(|_| anyhow!("host_native_skill_file"))?;
    ensure!(
        before.file_type().is_file() && before.len() > 0 && before.len() <= MAX_SKILL_BYTES,
        "host_native_skill_file"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options
        .open(&skill.path)
        .map_err(|_| anyhow!("host_native_skill_file"))?;
    let opened = file
        .metadata()
        .map_err(|_| anyhow!("host_native_skill_file"))?;
    ensure!(
        opened.is_file() && stable(&before, &opened),
        "host_native_skill_changed"
    );
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut scratch = [0_u8; 8192];
    loop {
        let count = file
            .read(&mut scratch)
            .map_err(|_| anyhow!("host_native_skill_file"))?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or_else(|| anyhow!("host_native_skill_file"))?;
        ensure!(bytes <= MAX_SKILL_BYTES, "host_native_skill_file");
        digest.update(&scratch[..count]);
    }
    let after = file
        .metadata()
        .map_err(|_| anyhow!("host_native_skill_file"))?;
    let current =
        fs::symlink_metadata(&skill.path).map_err(|_| anyhow!("host_native_skill_file"))?;
    ensure!(
        stable(&opened, &after)
            && stable(&after, &current)
            && current.file_type().is_file()
            && bytes == opened.len()
            && fs::canonicalize(&skill.path).map_err(|_| anyhow!("host_native_skill_path"))?
                == skill.path,
        "host_native_skill_changed"
    );
    ensure!(
        format!("{:x}", digest.finalize()) == skill.sha256,
        "host_native_skill_digest"
    );
    Ok(())
}

#[cfg(unix)]
fn stable(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
#[cfg(not(unix))]
fn stable(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

pub(crate) fn list_params(cwd: &Path) -> Value {
    json!({"cwds":[cwd],"forceReload":true})
}

/// Validate only the selected cwd and Skills. Unselected errors are not copied
/// into a response and cannot substitute for an exact selected name/path match.
pub(crate) fn observe_list(
    cwd: &Path,
    selected: &[NativeSkillInput],
    response: &Value,
) -> Result<Value> {
    let entries = response["data"]
        .as_array()
        .ok_or_else(|| anyhow!("host_native_skills_list_shape"))?;
    let mut matching = entries.iter().filter(|entry| {
        entry["cwd"]
            .as_str()
            .is_some_and(|path| Path::new(path) == cwd)
    });
    let entry = matching
        .next()
        .ok_or_else(|| anyhow!("host_native_skills_cwd"))?;
    ensure!(matching.next().is_none(), "host_native_skills_cwd");
    let skills = entry["skills"]
        .as_array()
        .ok_or_else(|| anyhow!("host_native_skills_list_shape"))?;
    let errors = entry["errors"]
        .as_array()
        .ok_or_else(|| anyhow!("host_native_skills_list_shape"))?;
    for skill in selected {
        for error in errors {
            let path = error["path"]
                .as_str()
                .ok_or_else(|| anyhow!("host_native_skills_list_shape"))?;
            ensure!(
                !skill.path.starts_with(Path::new(path)),
                "host_native_skill_list_error"
            );
        }
        let mut matching = skills.iter().filter(|entry| {
            entry["name"].as_str() == Some(skill.name.as_str())
                && entry["path"]
                    .as_str()
                    .is_some_and(|path| Path::new(path) == skill.path)
        });
        let entry = matching
            .next()
            .ok_or_else(|| anyhow!("host_native_skill_not_listed"))?;
        ensure!(
            matching.next().is_none(),
            "host_native_skill_list_ambiguous"
        );
        ensure!(entry["enabled"] == true, "host_native_skill_disabled");
    }
    Ok(json!({
        "schema_version":"gptgrep.native-skills-observation.v1",
        "source":"skills/list","cwd":cwd,"skills":selected,
        "exact_enabled_selection_observed":true,
        "sha256_source":"local_selected_file_before_turn",
        "input_type":"skill","body_loading_observed":false
    }))
}

pub(crate) fn append_inputs(input: &mut Value, selected: &[NativeSkillInput]) -> Result<()> {
    if selected.is_empty() {
        return Ok(());
    }
    // Recheck after listing, immediately before forming the native turn request.
    validate(selected)?;
    let input = input
        .as_array_mut()
        .ok_or_else(|| anyhow!("host_native_skill_input_shape"))?;
    input.extend(
        selected
            .iter()
            .map(|skill| json!({"type":"skill","name":skill.name,"path":skill.path})),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, PathBuf, NativeSkillInput)> {
        let temporary = tempfile::tempdir()?;
        let cwd = temporary.path().canonicalize()?;
        let directory = cwd.join(".agents/skills/synthetic");
        fs::create_dir_all(&directory)?;
        let path = directory.join("SKILL.md");
        let contents = b"---\nname: synthetic\ndescription: Synthetic test skill.\n---\nBody stays in the selected source.\n";
        fs::write(&path, contents)?;
        Ok((
            temporary,
            cwd,
            NativeSkillInput {
                name: "synthetic".into(),
                path,
                sha256: format!("{:x}", Sha256::digest(contents)),
            },
        ))
    }

    fn listing(cwd: &Path, skill: &NativeSkillInput) -> Value {
        json!({"data":[{"cwd":cwd,"skills":[{
            "name":skill.name,"path":skill.path,"enabled":true,
            "description":"Synthetic test skill.","scope":"repo","pluginId":null
        }],"errors":[]}]})
    }

    #[test]
    fn selected_files_require_unique_identity_canonical_path_and_exact_digest() -> Result<()> {
        let (_temporary, _cwd, skill) = fixture()?;
        validate(std::slice::from_ref(&skill))?;
        assert!(validate(&[skill.clone(), skill.clone()]).is_err());
        let mut wrong = skill.clone();
        wrong.sha256 = "f".repeat(64);
        assert!(validate(&[wrong]).is_err());
        fs::write(&skill.path, b"changed after selection")?;
        assert!(validate(&[skill]).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn selected_skill_symlink_is_not_a_canonical_file_pin() -> Result<()> {
        let (_temporary, cwd, mut skill) = fixture()?;
        let alias = cwd.join("alias");
        fs::create_dir(&alias)?;
        let path = alias.join("SKILL.md");
        std::os::unix::fs::symlink(&skill.path, &path)?;
        skill.path = path;
        assert!(validate(&[skill]).is_err());
        Ok(())
    }

    #[test]
    fn native_listing_requires_exact_enabled_selection_without_selected_errors() -> Result<()> {
        let (_temporary, cwd, skill) = fixture()?;
        let selected = [skill.clone()];
        let value = listing(&cwd, &skill);
        let receipt = observe_list(&cwd, &selected, &value)?;
        assert_eq!(receipt["skills"][0]["sha256"], skill.sha256);
        assert_eq!(receipt["body_loading_observed"], false);
        for field in ["name", "path", "enabled"] {
            let mut invalid = value.clone();
            invalid["data"][0]["skills"][0][field] = Value::Null;
            assert!(observe_list(&cwd, &selected, &invalid).is_err());
        }
        let mut invalid = value.clone();
        invalid["data"][0]["errors"] =
            json!([{"path":skill.path,"message":"private loader details"}]);
        let error = observe_list(&cwd, &selected, &invalid).unwrap_err();
        assert_eq!(error.to_string(), "host_native_skill_list_error");
        invalid["data"][0]["errors"] =
            json!([{"path":cwd.join("unselected/SKILL.md"),"message":"unselected"}]);
        observe_list(&cwd, &selected, &invalid)?;
        invalid["data"][0]["cwd"] = json!(cwd.join("other"));
        assert!(observe_list(&cwd, &selected, &invalid).is_err());
        Ok(())
    }

    #[test]
    fn native_skill_inputs_preserve_text_and_recheck_source_without_copying_body() -> Result<()> {
        let (_temporary, cwd, skill) = fixture()?;
        assert_eq!(list_params(&cwd), json!({"cwds":[cwd],"forceReload":true}));
        let mut input = json!([{"type":"text","text":"task","text_elements":[]}]);
        append_inputs(&mut input, std::slice::from_ref(&skill))?;
        assert_eq!(input[0]["text"], "task");
        assert_eq!(
            input[1],
            json!({"type":"skill","name":skill.name,"path":skill.path})
        );
        assert!(
            !input
                .to_string()
                .contains("Body stays in the selected source.")
        );
        fs::write(&skill.path, b"changed after listing")?;
        let mut next = json!([{"type":"text","text":"task","text_elements":[]}]);
        assert!(append_inputs(&mut next, &[skill]).is_err());
        assert_eq!(next.as_array().unwrap().len(), 1);
        Ok(())
    }
}
