# host-repo-backup

A small Rust CLI that lists a user's or organization's repositories and saves Git mirrors. Requires Git on PATH.

```sh
cargo build --release
./target/release/host-repo-backup --repo github.com/user ./backups
./target/release/host-repo-backup --repo codeberg.org/river ./backups
./target/release/host-repo-backup --repo gitlab.com/group ./backups
```

Mirrors live at `backups/HOST/OWNER/REPO.git`. Run the same command again to update them. Existing directories must be matching mirrors; ordinary checkouts are not overwritten. Individual Git failures are reported, other repositories are still attempted, and the command exits nonzero if any fail. API errors stop discovery before any mirrors are changed.

## Self-hosted services

GitHub.com and GitLab.com are detected automatically. Other hosts default to Forgejo, including Codeberg. Override the provider for self-hosted GitLab or GitHub Enterprise:

```sh
host-repo-backup --repo git.example.com/team/subgroup ./backups --provider gitlab
host-repo-backup --repo github.example.com/team ./backups --provider github
```

Use `HOST/USER_OR_ORG` or `https://HOST/USER_OR_ORG`. GitLab supports nested groups and includes their subgroups, excluding projects shared from other groups. Instances must be served at the host root over HTTPS.

## Private repositories

Set `GITHUB_TOKEN`, `GITLAB_TOKEN`, or `FORGEJO_TOKEN` for API discovery. Tokens are not saved in clone URLs or passed as Git command arguments. Configure Git's credential helper separately for HTTPS cloning of private repositories. Interactive Git password prompts are disabled.

GitHub lists public user repositories, plus private repositories when the supplied token belongs to that user. Organization and GitLab/Forgejo listings depend on token permissions. GitHub fine-grained tokens and organization SSO may need explicit repository access. Only repositories visible to the API credentials can be backed up.

## Scope

A mirror stores Git history, branches, and tags. It is not a backup of issues, pull requests, wikis, release assets, or Git LFS payloads. Mirrors fetch and prune, so upstream deleted refs disappear locally on the next run. Repositories removed from the API listing are left on disk. Use filesystem snapshots if you need historical backup retention.

```sh
# Restore a mirror to a new remote:
git -C ./backups/github.com/user/repo.git push --mirror https://example.com/user/repo.git

cargo test
cargo clippy -- -D warnings
```
