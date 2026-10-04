# Working in this repo

- After every change, commit it and push it to the current branch (`git push -u origin <branch>` the first time). Don't wait to be asked.
- Work on a feature branch, never directly on `main`. Merge to `main` only when asked.
- Never touch, stage or commit `reference(dont commit)/`.
- Before committing, make sure it builds and the tests pass: `cargo build --workspace`, `cargo test --workspace -- --test-threads=4`, `node --test "extension/test/*.test.mjs"`.
