# Fork settings

These repository settings are configured by the maintainer on GitHub. They cannot be verified from the code in this repository.

The repository is public and remains a GitHub fork, with upstream history, branches, tags and MPL-2.0 licensing retained.

The active default-branch ruleset has no bypass actors. It prohibits force pushes and deletion, requires a pull request, linear history, resolved review threads and the GitHub Actions "Fork CI" result against the latest base. Squash is the only allowed merge method. Auto-merge is disabled and merged branches are deleted.

Required approving reviews is zero because this is a single-maintainer repository: GitHub does not let an author approve their own PR. CODEOWNERS routes contributions to SenorLawyer. CI remains mandatory for the maintainer too.

Issues are enabled. Wiki and project boards are disabled. GitHub Actions tokens default to read access and cannot approve pull requests. Private vulnerability reporting and vulnerability alerts are enabled. Dependabot configuration covers Cargo and GitHub Actions. The release workflow creates draft GitHub releases and does not upload to upstream Roblox assets.
