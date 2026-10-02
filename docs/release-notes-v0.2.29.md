# Prismattyc 0.2.29 release notes

## Automatically install the pmux Agent Skill

On first launch after installation or an app version change, the macOS app
installs the pmux Agent Skill in the background for detected agents. The
Linux installer does the same after installation. Both preserve user-edited
copies and support opt-outs through configuration and
`PRISMATTYC_NO_AGENT_SKILLS=1`; the Linux installer also accepts
`--no-agent-skills`.

## Linux builds for glibc 2.28

Linux x86_64 and ARM64 builds now target glibc 2.28 or newer, including Amazon
Linux 2023.
