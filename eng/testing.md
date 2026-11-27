# Test architecture

## The gate

`just ci` is the single gate. It runs the Python hygiene checks, the documentation checks, formatting, Clippy, the workspace test suite, and the documentation build.

Pull requests and pushes to `main` run the same gate. CI runs the checks and the test suite as separate jobs, so the checks report while the tests still run. Split it into a fast pull-request gate and a full main gate when the total cost exceeds ten minutes on a hosted runner.

`just verify <directory> [recipe]` runs the gate, or the named recipe, recipe by recipe and retains the source snapshot, tool versions, per-recipe timings, and output outside the repository.

## Browser client

A browser-client change is checked through the built client at 1440, 1080, 900, 390, and 320 CSS pixels in English and Chinese. A layout-changing interaction is also checked with reduced motion. Screenshots and recordings stay in the pull request; the repository keeps no visual evidence.

## Test quality

Tests must catch meaningful failures, not serve as formalities.

- Assert behavior through the interface a caller uses: return values, errors, state, and external effects.
- Derive the expected result from the specification, not from the implementation.
- A test must fail when the behavior breaks and pass when the implementation changes and the behavior does not.
- Test accepted and rejected boundaries, not incidental error wording.
- Keep tests deterministic: fixed seeds, no sleeps, no wall-clock or ordering assumptions.

## Cost

A test that takes minutes has no value: an assertion nobody runs protects nothing. Measure a test's cost before adding it, and delete a slow test rather than quarantining it.

A change that adds more than 30 seconds to `just ci` reports the measured cost in its pull request.

## Placement

- Unit tests live in the module they test.
- Integration tests live in `tests/` and use the public interface.
- A doc test is for a public example that a reader will copy.

## Fixtures

Reuse a prepared fixture when another case in the same binary already prepared it for the same inputs. Build each fixture from the smallest deterministic input that exercises the behavior.

Do not weaken an assertion to keep a test fast. Delete the test instead, and record what it covered.
