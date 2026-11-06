# Contributing

Bug reports, questions and pull requests are welcome through
[GitHub Issues](https://github.com/tsconfigdotjson/vibe-os-native/issues) and pull
requests. Please leave API keys, tokens and private hostnames out of anything you
post, logs included.

Before changing behavior, read the relevant part of [PRD.md](PRD.md), the design
and source of truth. [docs/development.md](docs/development.md) covers the layout,
building and releases.

Pull requests are checked locally rather than by CI, which runs only on merges to
`main` or by hand:

```sh
scripts/check-full.sh
```

It runs on Linux and macOS. The macOS steps are skipped on Linux and need macOS 26.
Say in your pull request which you ran.

By contributing, you agree that your work is licensed under the
[MIT License](LICENSE).
