# Security policy

## Current status

clip-sync is in early development. No released version is currently considered suitable for sensitive clipboard contents. The protocol, cryptographic construction, storage format, and file-transfer behavior have not yet received an independent security review.

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability.

Use GitHub's private vulnerability reporting for this repository. Include:

- the affected commit or version;
- reproduction steps or a proof of concept;
- the expected impact;
- any suggested remediation;
- whether disclosure is time-sensitive.

You should receive an acknowledgement within seven days. Please allow a reasonable remediation window before public disclosure.

## Initial threat model

The planned first release assumes:

- devices are controlled by one user;
- peers communicate over a private NetBird network;
- a high-entropy shared mesh secret is provisioned out of band;
- local operating systems and the secret manager are trusted;
- every holder of the mesh secret has equal authority over shared history.

The first release does not attempt to protect against a compromised authorized peer, a compromised desktop session, clipboard-source application behavior, or plaintext that must temporarily exist while an item is active.
