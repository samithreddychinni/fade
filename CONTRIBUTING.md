# Contributing

Fade should feel small, direct, and reliable. Contributions are welcome when
they preserve that shape.

## Comment style

Use comments to explain why code exists, what invariant must hold, or what
external behavior is being protected. Do not comment code by repeating the
operation in different words.

Preferred:

```rust
// Expired files must fail lookup before the backing path is resolved.
```

Avoid:

```rust
// Check if the file is expired.
```

Rules:

- Use short sentence-case comments.
- Keep comments close to the behavior they explain.
- Prefer naming and small functions over explanatory comments when the code can
  be made obvious.
- Document filesystem edge cases, crash-recovery assumptions, and security
  boundaries explicitly.
- Do not log or comment with secret values, sample tokens, or realistic
  credentials.
- Use `TODO(name): reason` only when the follow-up is concrete.

## Commit message style

Use Conventional Commits:

```text
type(scope): summary
```

Examples:

```text
docs: add initial project documentation
feat(fuse): enforce expiry during lookup
fix(reaper): tolerate missing backing files
test(policy): cover first-match rule precedence
```

Rules:

- Use one of: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `build`, `ci`,
  or `chore`.
- Keep the subject under 72 characters.
- Use the imperative mood: `add`, `fix`, `reject`, `document`.
- Do not end the subject with a period.
- Add a body when the reason, tradeoff, or migration path matters.
- Keep the body wrapped at roughly 72 characters.
- No emoji in commit messages.

