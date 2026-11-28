# Live update result: proof

`tests/native/live-update-result-e2e.py`, in a fresh macOS 26.6.2 arm64 VM,
built in the VM at c747ab5f and deleted after. PASS, twice.

1. A private pmux server runs `default`, `alpha` and `beta`. The update
   flow's own restart (`pmux restart --daemon --stop-sessions
   --consented-config ... --worker --json`) hands them to the new server
   live. The receipt (`macos-receipt.json`) reports `updated_live`, and the
   alpha and beta shells keep their process IDs.
2. The real app is launched the way the update relaunches it, with the
   resume record carrying that receipt's sessions. Its result panel
   (`macos-live-result.png`) says the three sessions kept running through
   the update. It does not say they were restored or that their programs
   started again.

The notice's first line is the text the updater writes after a live update.
The script supplies it, because the updater itself installs a published
release. `update_flow::tests::a_live_update_says_the_sessions_kept_running`
checks that line and the fallback "The daemon restarted on …" wording.
`a_captured_live_receipt_reads_as_kept_running` runs the app's parser on the
receipt captured here.
