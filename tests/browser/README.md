# Optional real-browser regression

Requires Python 3 (standard library), a built rustglance binary and Chromium.
Uses only a local HTTP fixture and a disposable fresh browser profile.
Does not use accounts, public websites, or personal sessions. Run separately
from unit tests; a browser is actually launched headless.

```text
python tests/browser/readiness.py --exe target/release/rustglance --chromium /path/to/chromium --output /path/to/results
```

On Windows use `target/release/rustglance.exe` and an explicit Chrome/Edge path.
Results and the RG journal are retained under the output folder; only the
test-owned temporary profile is cleaned up. Tests cover default result shape,
useful content despite unrelated work, selector validation before action,
click navigation, missing/hidden/unstable content, bounded wait without tab
recovery, document freshness and blocked POST. No claim of full browser or
security coverage.
