# Changelog

## v0.2.0 — 2026-05-29

Add `wm ready` — a standing device-readiness beacon that joins the
fleet-install-doctor per-unit verdict with what a *working* companion
needs: API key present (or degrade-configured), an audio source and
sink, agorabus reachable, and `wintermute.target` active. Speaks the
verdict in plain language on boot and emits a `wm.health.ready`
envelope for off-device beaconing. Distinct from companion-degrade's
mid-conversation failure voice — this is the deploy/boot readiness voice.
