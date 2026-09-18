# App screenshots

`overview.png`, `sleep.png`, and `activity.png` were captured on September 16,
2026 from the production SwiftUI app on the iPhone 17 Pro simulator (iOS 26.5,
dark appearance), running against the author's own Ring 5 data as synced to
the local store (last sync July 17, 2026). No ring identifiers or keys are
shown. Images are unretouched simulator screenshots, opened with the
`-openDay` launch argument.

## Pairing and symptom radar — September 8, 2026

`pairing-dark.png`, `pairing-sync-dark.png`, and `pairing-help-light.png` show
setup, active sync, and ring-discovery guidance. `symptom-radar-light.png` and
`symptom-radar-dark.png` show the simplified radar with synthetic minor-signs
biometrics. Captured on the same iPhone 17 Pro / iOS 26.5 simulator.

A separate temporary preview app presents the production SwiftUI views with
controlled state; the shipped entry point is unchanged. No real keys or health
data are included. Screenshots are unretouched. The views were also checked at
an accessibility text size, and UI automation exercised key validation and
visibility, diagnostics, reset cancellation, pause, failure guidance, and the
personal-range toggle.

## Compact radar and day refresh, September 9, 2026

- [Collapsed radar](symptom-radar-collapsed-dark.png)
- [Expanded measurements and personal ranges](symptom-radar-expanded-dark.png)
- [Support panel](support-panel-dark.png)
- [Historical activity refresh](day-analysis-refresh-dark.png)

These unretouched iPhone 17 Pro simulator captures use synthetic data and the
production SwiftUI views in a temporary preview app. UI checks cover disclosure,
missing data, accessibility text size, support actions, reset cancellation, and
refreshing both tabs through the historical-day browser. Refresh callbacks in the
preview simulate success and failure to verify live updates and retained results.
The app's native test suite separately runs the actual activity model to verify
that refreshing an older day bypasses its cache and preserves other days.
