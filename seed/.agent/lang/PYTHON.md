# Python add-on

Language conventions layered on top of `.agent/STANDARDS.md`. Pulled in when the
repo contains Python.

## Build & checks
- The done bar for Python: `ruff check` + `ruff format --check`, type-check (`mypy` or `pyright`) clean on changed code, and `pytest` green.
- Pin the interpreter and dependencies (`uv`/`pyproject.toml` with a lockfile). Don't `pip install` into a global environment.

## Idioms
- Type-annotate public functions and dataclasses; the type checker is part of the done bar, not optional.
- Prefer `dataclass`/`pydantic` models over passing around bare dicts. Validate at the boundary, trust internally.
- Use `pathlib`, context managers for resources, and f-strings. Avoid mutable default arguments.
- Raise specific exceptions; never `except:` bare or swallow errors silently. Log with the stdlib `logging` module, not `print`, in library/service code.

## Tests
- `pytest` with fixtures over setup/teardown boilerplate. Parametrize instead of copy-pasting cases. A bug fix starts with a failing test that reproduces it.

## Security
- Never build SQL or shell strings by concatenation — parameterize. Validate and bound all external input.
- Secrets come from the environment / Frontkeep's secret tools, never from source or notebooks committed to the repo.
