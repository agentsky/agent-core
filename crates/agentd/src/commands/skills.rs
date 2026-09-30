//! The skill commands: `skill add`, `skill confirm` and `skill rm`, owner
//! only, like every agent command.

use commands::SkillCommand;
use core_types::{InFile, MemberKey};
use store::SkillState;

use super::agents::{Download, no_such_agent};
use super::{Commands, Failure, Origin};
use crate::skills::package::{MAX_SKILL_BYTES, MAX_SKILL_MD_BYTES};
use crate::skills::{Added, BUNDLED_NAME, Confirmed, Manifest, PENDING_TTL, Skills, Source};

impl Commands {
    pub(super) async fn skill(
        &self,
        skills: &Skills,
        key: &MemberKey,
        command: SkillCommand,
        origin: &Origin,
        files: &[InFile],
    ) -> Result<String, Failure> {
        let (SkillCommand::Add { name, .. }
        | SkillCommand::Confirm { name, .. }
        | SkillCommand::Rm { name, .. }) = &command;
        let name = name.as_str();
        let Some(agent) = self.own_agent(key, name).await? else {
            return Ok(no_such_agent(name));
        };
        match &command {
            SkillCommand::Add { source, .. } => {
                let upload;
                let source = match source {
                    Some(url) => Source::Git(url),
                    None => match self.attached_skill(key, origin, files).await? {
                        Ok(file) => {
                            upload = file;
                            Source::Upload {
                                name: &upload.0,
                                bytes: &upload.1,
                            }
                        }
                        Err(problem) => return Ok(problem),
                    },
                };
                Ok(match skills.add(agent.id, source, agent.owner).await? {
                    Ok(Added::Active(manifest)) => format!(
                        "Added the skill `{}` to `{name}`. Its conversations use it from their \
                         next start.",
                        manifest.name
                    ),
                    Ok(Added::Pending(manifest)) => pending_reply(name, &manifest, origin),
                    Err(refused) => refused.to_string(),
                })
            }
            SkillCommand::Confirm { skill, .. } => {
                Ok(match skills.confirm(agent.id, skill.as_str()).await? {
                    Confirmed::Active(row) => format!(
                        "Added the skill `{skill}` to `{name}`. Its sandboxes may now reach {}, \
                         and its conversations use it from their next start.",
                        list(&row.hosts)
                    ),
                    Confirmed::NotPending => format!(
                        "`{name}` has no skill `{skill}` waiting for you to confirm its hosts."
                    ),
                    Confirmed::Expired => format!(
                        "The skill `{skill}` waited more than {} minutes, so I dropped it. Add \
                         it again with {}.",
                        PENDING_TTL.as_secs() / 60,
                        origin.command(&format!("skill add {name}"))
                    ),
                })
            }
            SkillCommand::Rm { skill, .. } => {
                if skill.as_str() == BUNDLED_NAME {
                    return Ok(format!(
                        "`{BUNDLED_NAME}` is built into every agent and can't be removed."
                    ));
                }
                if skills.remove(agent.id, skill.as_str()).await? {
                    return Ok(format!(
                        "Removed the skill `{skill}` from `{name}`, with any hosts it let the \
                         agent reach. Conversations running now keep it until they next start."
                    ));
                }
                let names: Vec<String> = skills
                    .list(agent.id)
                    .await?
                    .into_iter()
                    .filter(|s| s.state == SkillState::Active)
                    .map(|s| format!("`{}`", s.name))
                    .collect();
                Ok(if names.is_empty() {
                    format!("`{name}` has no skill `{skill}`, and no skills besides `agentctl`.")
                } else {
                    format!(
                        "`{name}` has no skill `{skill}`. Its skills: {}.",
                        names.join(", ")
                    )
                })
            }
        }
    }

    /// The name and bytes of the skill file attached to `skill add`, or
    /// what is wrong.
    async fn attached_skill(
        &self,
        key: &MemberKey,
        origin: &Origin,
        files: &[InFile],
    ) -> Result<Result<(String, Vec<u8>), String>, Failure> {
        let how = "Give the skill's https:// Git URL after the agent's name, or attach its \
                   SKILL.md, or a .zip holding it, to that command in a direct message with me.";
        let [file] = files else {
            return Ok(Err(how.to_owned()));
        };
        let lower = file.name.to_ascii_lowercase();
        let max = if lower.ends_with(".md") {
            MAX_SKILL_MD_BYTES
        } else if lower.ends_with(".zip") {
            MAX_SKILL_BYTES
        } else {
            return Ok(Err(format!(
                "Attach the skill as its SKILL.md, or as a .zip holding it. {how}"
            )));
        };
        Ok(match self.download(key, origin, file, max).await? {
            Download::Bytes(bytes) => Ok((file.name.clone(), bytes)),
            Download::TooLarge => Err(format!("That file is over the {} limit.", size(max))),
            Download::NotHere => Err(how.to_owned()),
        })
    }
}

/// The reply to a skill held back for its hosts.
fn pending_reply(agent: &str, manifest: &Manifest, origin: &Origin) -> String {
    let hosts: Vec<String> = manifest.hosts.iter().map(ToString::to_string).collect();
    format!(
        "The skill `{skill}` asks that `{agent}`'s sandboxes may reach {hosts}. Anything the \
         agent can read could be sent there, so I haven't added it yet. To add it with those \
         hosts, send {confirm} within {minutes} minutes.",
        skill = manifest.name,
        hosts = list(&hosts),
        confirm = origin.command(&format!("skill confirm {agent} {}", manifest.name)),
        minutes = PENDING_TTL.as_secs() / 60,
    )
}

/// `bytes` in MB when it is a whole number of them, else in KB.
fn size(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    if bytes.is_multiple_of(MB) {
        format!("{} MB", bytes / MB)
    } else {
        format!("{} KB", bytes / 1024)
    }
}

/// `hosts` as Markdown code, joined for a sentence.
fn list(hosts: &[String]) -> String {
    let hosts: Vec<String> = hosts
        .iter()
        .map(|host| format!("`{}`", host.replace('`', "")))
        .collect();
    match hosts.as_slice() {
        [] => "no other hosts".to_owned(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_mb_or_kb() {
        assert_eq!(size(10 * 1024 * 1024), "10 MB");
        assert_eq!(size(256 * 1024), "256 KB");
    }

    #[test]
    fn hosts_read_as_a_sentence() {
        assert_eq!(list(&[]), "no other hosts");
        assert_eq!(list(&["a.io".into()]), "`a.io`");
        assert_eq!(
            list(&["a.io".into(), "b.io".into(), "*.c.io:8443".into()]),
            "`a.io`, `b.io` and `*.c.io:8443`"
        );
    }
}
