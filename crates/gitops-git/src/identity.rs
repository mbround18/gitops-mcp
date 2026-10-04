//! Resolving the identity a signing key declares about itself.

use std::path::Path;

use crate::{Error, Result, runner::CommandRunner, status::SigningIdentity};

/// Read the identity of `key` under the given `gpg.format`.
pub fn resolve(
    runner: &dyn CommandRunner,
    cwd: Option<&Path>,
    format: &str,
    key: &str,
) -> Result<SigningIdentity> {
    match format {
        "openpgp" => openpgp(runner, cwd, key),
        "ssh" => ssh(runner, cwd, key),
        other => Err(Error::UnsupportedSigningFormat(other.to_owned())),
    }
}

fn openpgp(runner: &dyn CommandRunner, cwd: Option<&Path>, key: &str) -> Result<SigningIdentity> {
    let public = runner.run("gpg", &["--list-keys", "--with-colons", key], cwd)?;
    if !public.ok() {
        return Err(Error::KeyNotFound {
            key: key.to_owned(),
            detail: first_line(&public.stderr),
        });
    }
    let (name, email) = parse_gpg_uid(&public.stdout);
    let secret_key_present = runner
        .run("gpg", &["--list-secret-keys", "--with-colons", key], cwd)?
        .ok();

    Ok(SigningIdentity {
        format: "openpgp".into(),
        key: key.to_owned(),
        email,
        name,
        secret_key_present,
    })
}

fn ssh(runner: &dyn CommandRunner, cwd: Option<&Path>, key: &str) -> Result<SigningIdentity> {
    // `user.signingkey` may hold the key material inline (`ssh-ed25519 AAAA... comment`)
    // or a path to the public key file.
    let (comment, secret_key_present) = if key.starts_with("ssh-") || key.starts_with("sk-ssh-") {
        (inline_ssh_comment(key), true)
    } else {
        let path = key.trim_start_matches("key::");
        let listed = runner.run("ssh-keygen", &["-l", "-f", path], cwd)?;
        if !listed.ok() {
            return Err(Error::KeyNotFound {
                key: key.to_owned(),
                detail: first_line(&listed.stderr),
            });
        }
        (parse_ssh_keygen_comment(&listed.stdout), true)
    };

    // An SSH signing key carries no structured uid: the comment is the only identity
    // it declares, and it is only an identity if it looks like an email address.
    let email = comment.filter(|c| c.contains('@') && !c.contains(' '));

    Ok(SigningIdentity {
        format: "ssh".into(),
        key: key.to_owned(),
        email,
        name: None,
        secret_key_present,
    })
}

/// Pull name and email out of the first `uid:` record of `gpg --with-colons` output.
fn parse_gpg_uid(stdout: &str) -> (Option<String>, Option<String>) {
    for line in stdout.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some("uid") {
            continue;
        }
        let Some(uid) = fields.nth(8) else { continue };
        let email = uid
            .split_once('<')
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(addr, _)| addr.trim().to_owned());
        let name = uid
            .split('<')
            .next()
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty());
        return (name, email);
    }
    (None, None)
}

/// `256 SHA256:abc… the-comment (ED25519)` -> `the-comment`
fn parse_ssh_keygen_comment(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?;
    let rest = line
        .split_whitespace()
        .skip(2)
        .collect::<Vec<_>>()
        .join(" ");
    let comment = rest
        .rsplit_once(" (")
        .map(|(c, _)| c)
        .unwrap_or(&rest)
        .trim();
    (!comment.is_empty() && comment != "no comment").then(|| comment.to_owned())
}

/// `ssh-ed25519 AAAA… the-comment` -> `the-comment`
fn inline_ssh_comment(key: &str) -> Option<String> {
    let comment = key.split_whitespace().nth(2)?.trim();
    (!comment.is_empty()).then(|| comment.to_owned())
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GPG_COLONS: &str = "tru::1:1700000000:0:3:1:5\n\
pub:u:4096:1:354F34B4DB349BD6:1711036800:::u:::scESC::::::23::0:\n\
fpr:::::::::415A0A84194499B32E4CF7F1354F34B4DB349BD6:\n\
uid:u::::1711036800::ABC123::Michael Bruno <michael.bruno1337@gmail.com>::::::::::0:\n\
sub:u:4096:1:48338CAD143699C7:1711036800::::::e::::::23:\n";

    #[test]
    fn parses_name_and_email_from_gpg_uid() {
        let (name, email) = parse_gpg_uid(GPG_COLONS);
        assert_eq!(name.as_deref(), Some("Michael Bruno"));
        assert_eq!(email.as_deref(), Some("michael.bruno1337@gmail.com"));
    }

    #[test]
    fn missing_uid_yields_no_identity() {
        assert_eq!(
            parse_gpg_uid("pub:u:4096:1:DEADBEEF:0:::u:::scESC:\n"),
            (None, None)
        );
    }

    #[test]
    fn parses_ssh_keygen_comment() {
        let out = "256 SHA256:7d1Q+abc mbruno@example.com (ED25519)\n";
        assert_eq!(
            parse_ssh_keygen_comment(out).as_deref(),
            Some("mbruno@example.com")
        );
    }

    #[test]
    fn parses_inline_ssh_comment() {
        assert_eq!(
            inline_ssh_comment("ssh-ed25519 AAAAC3NzaC1lZDI1 mbruno@example.com").as_deref(),
            Some("mbruno@example.com")
        );
    }
}
