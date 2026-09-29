use reqwest::{StatusCode, Url, blocking::Client, redirect::Policy};
use serde_json::Value;
use std::{env, error::Error, fs, path::Path, process::Command, time::Duration};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const HELP: &str =
    "Usage: host-repo-backup --repo HOST/USER_OR_ORG PATH [--provider github|gitlab|forgejo]

Examples:
  host-repo-backup --repo github.com/user ./backups
  host-repo-backup --repo codeberg.org/river ./backups
  host-repo-backup --repo git.example.com/team/subgroup ./backups --provider gitlab

GitHub and GitLab.com are detected by hostname; other hosts default to Forgejo.
Optional API tokens: GITHUB_TOKEN, GITLAB_TOKEN, FORGEJO_TOKEN.
Git uses your existing credential helper for private repositories.
Backups are Git mirrors: branches and tags, not issues, LFS objects or release assets.
Reruns fetch and prune mirrors. Deleted upstream refs are removed locally too.";

struct Source {
    url: Url,
    namespace: String,
    provider: String,
}

impl Source {
    fn parse(input: &str, provider: Option<String>) -> Result<Self> {
        let url = Url::parse(&format!(
            "https://{}",
            input.strip_prefix("https://").unwrap_or(input)
        ))?;
        let namespace = url.path().trim_matches('/').to_owned();
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !safe_namespace(&namespace)
        {
            return Err(
                "use HOST/USER_OR_ORG (HTTPS only, no credentials, query or fragment)".into(),
            );
        }
        let provider = provider.unwrap_or_else(|| {
            match url.host_str().unwrap() {
                "github.com" => "github",
                "gitlab.com" => "gitlab",
                _ => "forgejo",
            }
            .to_owned()
        });
        if !matches!(provider.as_str(), "github" | "gitlab" | "forgejo") {
            return Err("provider must be github, gitlab or forgejo".into());
        }
        if provider != "gitlab" && namespace.contains('/') {
            return Err("only GitLab supports nested group paths".into());
        }
        Ok(Self {
            url,
            namespace,
            provider,
        })
    }

    fn api(&self, segments: &[&str]) -> Url {
        let mut url = if self.url.host_str() == Some("github.com") && self.provider == "github" {
            Url::parse("https://api.github.com").unwrap()
        } else {
            self.url.clone()
        };
        let mut path = url.path_segments_mut().unwrap();
        path.clear();
        match self.provider.as_str() {
            "github" if self.url.host_str() != Some("github.com") => {
                path.extend(["api", "v3"]);
            }
            "gitlab" => {
                path.extend(["api", "v4"]);
            }
            "forgejo" => {
                path.extend(["api", "v1"]);
            }
            _ => {}
        }
        path.extend(segments);
        drop(path);
        url
    }
}

fn safe_namespace(value: &str) -> bool {
    value.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    })
}

fn get(client: &Client, source: &Source, url: Url, token: Option<&str>) -> Result<Option<Value>> {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = if source.provider == "gitlab" {
            request.header("PRIVATE-TOKEN", token)
        } else {
            request.bearer_auth(token)
        };
    }
    let response = request.send()?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(response.error_for_status()?.json()?))
}

fn repositories(
    client: &Client,
    source: &Source,
    token: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let owner = source.namespace.as_str();
    let mut endpoint = match source.provider.as_str() {
        "gitlab" => source.api(&["groups", owner, "projects"]),
        _ => source.api(&["orgs", owner, "repos"]),
    };
    if source.provider == "gitlab" {
        endpoint
            .query_pairs_mut()
            .append_pair("include_subgroups", "true")
            .append_pair("with_shared", "false");
    }
    let page_size = if source.provider == "forgejo" {
        "50"
    } else {
        "100"
    };
    let mut repos = Vec::new();
    let mut page = 1;
    let mut fallback = false;
    loop {
        let mut url = endpoint.clone();
        url.query_pairs_mut().append_pair("page", &page.to_string());
        url.query_pairs_mut().append_pair(
            if source.provider == "forgejo" {
                "limit"
            } else {
                "per_page"
            },
            page_size,
        );
        let data = match get(client, source, url, token)? {
            Some(data) => data,
            None if page == 1 && !fallback => {
                fallback = true;
                endpoint = if source.provider == "gitlab" {
                    let mut lookup = source.api(&["users"]);
                    lookup.query_pairs_mut().append_pair("username", owner);
                    let users = get(client, source, lookup, token)?.ok_or("user lookup failed")?;
                    let id = users
                        .as_array()
                        .and_then(|users| users.first())
                        .and_then(|user| user["id"].as_u64())
                        .ok_or("user or group not found")?;
                    source.api(&["users", &id.to_string(), "projects"])
                } else if source.provider == "github" && token.is_some() {
                    let user = get(client, source, source.api(&["user"]), token)?
                        .ok_or("token user not found")?;
                    if user["login"]
                        .as_str()
                        .is_some_and(|login| login.eq_ignore_ascii_case(owner))
                    {
                        let mut url = source.api(&["user", "repos"]);
                        url.query_pairs_mut().append_pair("affiliation", "owner");
                        url
                    } else {
                        source.api(&["users", owner, "repos"])
                    }
                } else {
                    source.api(&["users", owner, "repos"])
                };
                continue;
            }
            None => {
                return Err(
                    "user, organization or repository listing not found (check token permissions)"
                        .into(),
                );
            }
        };
        let entries = data.as_array().ok_or("expected repository list from API")?;
        if entries.is_empty() {
            break;
        }
        for repo in entries {
            let (name_key, url_key) = match source.provider.as_str() {
                "gitlab" => ("path_with_namespace", "http_url_to_repo"),
                _ => ("full_name", "clone_url"),
            };
            let name = repo[name_key]
                .as_str()
                .ok_or("repository missing namespace")?;
            let clone_url = repo[url_key]
                .as_str()
                .ok_or("repository missing clone URL")?;
            let url = Url::parse(clone_url)?;
            if !safe_namespace(name)
                || url.origin() != source.url.origin()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(format!("unsafe repository name or clone URL: {name}").into());
            }
            repos.push((name.to_owned(), clone_url.to_owned()));
        }
        page += 1;
    }
    Ok(repos)
}

fn git_output(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn backup(path: &Path, clone_url: &str) -> Result<()> {
    // Reject symlinks in the destination tree instead of following them outside the backup directory.
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("refusing symlink: {}", ancestor.display()).into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if path.exists() {
        if git_output(path, &["rev-parse", "--is-bare-repository"])? != "true"
            || git_output(path, &["remote", "get-url", "origin"])? != clone_url
            || git_output(path, &["config", "--get", "remote.origin.mirror"])? != "true"
            || git_output(path, &["config", "--get-all", "remote.origin.fetch"])?
                != "+refs/*:refs/*"
        {
            return Err(
                "destination is not a matching Git mirror; refusing to overwrite it".into(),
            );
        }
    } else {
        fs::create_dir_all(path.parent().ok_or("destination has no parent")?)?;
    }
    let mut command = Command::new("git");
    if path.exists() {
        command
            .arg("-C")
            .arg(path)
            .args(["fetch", "--prune", "origin"]);
    } else {
        command
            .args(["clone", "--mirror", "--", clone_url])
            .arg(path);
    }
    if !command.env("GIT_TERMINAL_PROMPT", "0").status()?.success() {
        return Err("Git backup failed; check network and Git credentials".into());
    }
    Ok(())
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let mut repo = None;
    let mut provider = None;
    let mut destination = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
            }
            "--repo" if repo.is_none() => {
                repo = Some(args.next().ok_or("--repo needs HOST/USER_OR_ORG")?)
            }
            "--provider" if provider.is_none() => {
                provider = Some(args.next().ok_or("--provider needs a value")?)
            }
            _ if arg.starts_with('-') => {
                return Err(format!("unknown or repeated option: {arg}").into());
            }
            _ if destination.is_none() => destination = Some(arg),
            _ => return Err("only one destination path is allowed".into()),
        }
    }
    let source = Source::parse(&repo.ok_or(HELP)?, provider)?;
    let destination = destination.ok_or(HELP)?;
    let root = std::path::absolute(destination)?.join(source.url.authority());
    let token_name = format!("{}_TOKEN", source.provider.to_uppercase());
    let token = env::var(&token_name).ok().filter(|value| !value.is_empty());
    let client = Client::builder()
        .user_agent(concat!("host-repo-backup/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::none())
        .timeout(Duration::from_secs(60))
        .build()?;
    Command::new("git").arg("--version").output()?;
    let repos = repositories(&client, &source, token.as_deref())?;
    println!("Found {} repositories", repos.len());
    let mut failures = 0;
    for (name, clone_url) in &repos {
        let path = root.join(format!("{name}.git"));
        println!("Backing up {name} -> {}", path.display());
        if let Err(error) = backup(&path, clone_url) {
            eprintln!("{name}: {error}");
            failures += 1;
        }
    }
    if failures > 0 {
        return Err(format!("{failures} of {} backups failed", repos.len()).into());
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_and_destination_validation() {
        let github = Source::parse("github.com/alice", None).unwrap();
        assert_eq!(
            github.api(&["users", "alice", "repos"]).as_str(),
            "https://api.github.com/users/alice/repos"
        );
        let forgejo = Source::parse("https://codeberg.org/river/", None).unwrap();
        assert_eq!(
            forgejo.api(&["orgs", "river", "repos"]).as_str(),
            "https://codeberg.org/api/v1/orgs/river/repos"
        );
        let gitlab = Source::parse("git.example.com/team/subgroup", Some("gitlab".into())).unwrap();
        assert_eq!(
            gitlab
                .api(&["groups", &gitlab.namespace, "projects"])
                .as_str(),
            "https://git.example.com/api/v4/groups/team%2Fsubgroup/projects"
        );
        for input in [
            "github.com",
            "github.com/a/b",
            "github.com/a?token=x",
            "user:secret@github.com/a",
            "http://github.com/a",
            "github.com/a%2Fb",
        ] {
            assert!(Source::parse(input, None).is_err(), "{input}");
        }
        for name in [
            "../repo",
            "a/../repo",
            "a//repo",
            "/absolute",
            "a/repo\\evil",
            "a/./repo",
        ] {
            assert!(!safe_namespace(name), "{name}");
        }
        assert!(safe_namespace("team/subgroup/my_repo-1.2"));
    }
}
