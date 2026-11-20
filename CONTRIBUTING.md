# Contributing

Early alpha, so the most useful thing is a report: what you tried, on which Link and Jellyfin version, what happened,
and the relevant part of `/mnt/scratch/sljf/sljfd.log`. **Remove your server URL and token from logs before posting.**

Pull requests welcome. Please:

- keep anything network-facing off by default and allow-listed in config;
- keep secrets out of URLs, argv and files on the build machine;
- say how you tested on a real Link (tools in [docs/DEVELOPING.md](docs/DEVELOPING.md));
- for new inputs or integrations, follow [docs/integrations/README.md](docs/integrations/README.md);
- work on a branch, open a PR, add a CHANGELOG line under [Unreleased]; `main` is merged by squash once CI passes.
  Versions, branches and releases: [docs/RELEASING.md](docs/RELEASING.md).

By contributing you agree your contribution is licensed GPL-3.0-or-later.
