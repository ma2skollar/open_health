# App screenshots

`overview.png`, `sleep.png`, and `activity.png` were captured from the SwiftUI
app at `9f21660` on the iPhone 17 Pro simulator
(iOS 26.5, light appearance). The screens use deterministic synthetic data;
no personal health records or ring identifiers are included.

A temporary capture build supplies the demo summary, disables automatic sync
and opens the existing overview, sleep and activity views. The production app
is unchanged. Images are unretouched simulator screenshots.

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
