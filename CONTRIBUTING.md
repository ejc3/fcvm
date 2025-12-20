# Contributing to fcvm

Welcome! We're excited you're interested in contributing to fcvm. Whether you're fixing a typo, reporting a bug, or implementing a new feature, your contribution is valued.

## Getting Started

1. **Fork and clone** the repo
2. **Set up dependencies** - see the [Required Forks](README.md#required-forks) section in the README
3. **Build** with `make build`
4. **Run tests** with `make test`

## Ways to Contribute

### Report Bugs

Found something broken? [Open an issue](https://github.com/ejc3/fcvm/issues/new). Include:
- What you expected to happen
- What actually happened
- Steps to reproduce
- Your environment (OS, architecture, etc.)

### Suggest Features

Have an idea? [Open an issue](https://github.com/ejc3/fcvm/issues/new) describing:
- The problem you're trying to solve
- Your proposed solution
- Any alternatives you considered

### Submit Code

1. Check [open issues](https://github.com/ejc3/fcvm/issues) for something to work on
2. Comment on the issue to let others know you're working on it
3. Fork, branch, and make your changes
4. Run `make lint` and `make test` before submitting
5. Open a pull request

## Development Workflow

```bash
# Build everything
make build

# Run lints (must pass before PR)
make lint

# Run tests
make test              # fuse-pipe tests
make test-vm           # VM integration tests (requires KVM)

# Format code
make fmt
```

## Code Style

- Run `cargo fmt` before committing
- Run `cargo clippy` and fix warnings
- Keep changes focused - one feature/fix per PR
- Add tests for new functionality

## Questions?

Open an issue! There are no dumb questions.
