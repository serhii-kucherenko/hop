# Security Policy

## Supported status

SideShift is currently MVP-stage software and not yet hardened for hostile networks.

## Trust model

SideShift is intended for a **trusted private LAN**:
- peers are manually configured
- both sides must share a passphrase
- control and input packets are authenticated/encrypted

This does **not** guarantee safety on an untrusted network.

## Operational guidance

- Use a long, unique shared secret
- Keep SideShift traffic on private/home/office LAN segments
- Restrict firewall rules to known peer IPs where possible
- Do not expose SideShift ports directly to the public internet

## Known MVP limitations

- No certificate identity infrastructure yet
- No automatic key rotation
- No multi-factor peer enrollment flow
- No telemetry-backed intrusion detection

## Reporting a vulnerability

Please open a private security report through GitHub Security Advisories if possible.

If private reporting is not available, open a GitHub issue with minimal exploit detail and request private follow-up.
