#!/usr/bin/env python3
"""Write and submit a prompt to an isolated agent pane, then show its reply."""
import json
import os
import re
import socket
import subprocess
import time
from pathlib import Path


PMUX = os.environ.get("PRISMATTYC_DEMO_PMUX", "pmux")
PMUX_SOCKET = os.environ["PMUX_SOCKET"]
TASK = os.environ.get("PRISMATTYC_DEMO_TASK", "reply")
AGENT = "codex" if TASK == "mail" else os.environ.get("PRISMATTYC_DEMO_AGENT", "codex")
if AGENT not in {"codex", "muse"}:
    raise ValueError("PRISMATTYC_DEMO_AGENT must be codex or muse")
PROMPTS = {
    "mail": (
        'Use pmux_send once: to "muse", summary "hello from Codex", body '
        '"Muse, in one sentence, what can you see from your session? Reply with '
        'pmux_send to agent id codex." Then say sent and stop.'
    ),
    "claim": "PMUX_MAIL",
    "reply": "Reply with exactly PANE_WRITE_OK and nothing else.",
}
try:
    PROMPT = PROMPTS[TASK]
except KeyError as error:
    raise ValueError("PRISMATTYC_DEMO_TASK must be mail, claim, or reply") from error
REPLY = "PANE_WRITE_OK"
TIMEOUT = int(os.environ.get("PRISMATTYC_DEMO_TIMEOUT", "120"))


def run(*args):
    return subprocess.check_output(
        [PMUX, "--socket", PMUX_SOCKET, *args], text=True
    ).strip()


def agent_pane():
    with socket.socket(socket.AF_UNIX) as stream:
        stream.connect(PMUX_SOCKET)
        stream.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
        snapshot = json.loads(stream.makefile().readline())["response"]["snapshot"]
    session = next(s for s in snapshot["sessions"] if s["name"] == AGENT)
    return session["windows"][0]["panes"][0]["id"]


def visible_reply(pane):
    return run("save-buffer", str(pane), "-").count(REPLY) >= 2


def prompt_is_still_in_composer(pane):
    if visible_reply(pane):
        return False
    screen = run("save-buffer", str(pane), "-")
    tail = " ".join(screen.splitlines()[-12:])
    prompt = " ".join(PROMPT.split())
    empty_composer = any(
        placeholder in tail
        for placeholder in ("Ask Codex to do anything", "Ask Muse to do anything")
    )
    return prompt in tail and not empty_composer


def mail_depth(agent):
    text = run("mail", "--as", agent, "inbox")
    counts = [int(value) for value in re.findall(r"\d+", text)]
    return sum(counts[:2]) if counts else 0


def mail_open(agent):
    text = run("mail", "--as", agent, "inbox")
    match = re.search(r"open:\s*(\d+)", text)
    return int(match.group(1)) if match else 0


def result_visible(pane):
    if TASK == "mail":
        return mail_depth("muse") > 0 or mail_depth("codex") > 0
    if TASK == "claim":
        return mail_open("muse") == 0
    return visible_reply(pane)


def send_literal_enter(pane):
    # The carriage return is a distinct argument, separate from the pane text.
    # --force is needed if the TUI still reports the composer as dirty.
    result = subprocess.run(
        [PMUX, "--socket", PMUX_SOCKET, "send", str(pane), "\r", "--literal", "--force"],
        text=True,
        capture_output=True,
        check=True,
    )
    print(result.stdout.strip(), flush=True)


def demonstrate():
    if TASK == "claim" and mail_open("muse") == 0:
        raise RuntimeError("Muse has no open mail to claim")
    pane = agent_pane()
    print(f"Target: isolated {AGENT.title()} pane {pane}.", flush=True)
    print(
        f"pmux pane-write {pane} --text '{PROMPT}' --submit none --json",
        flush=True,
    )
    receipt_text = run(
        "pane-write", str(pane), "--text", PROMPT, "--submit", "none", "--json"
    )
    receipt = json.loads(receipt_text)
    if receipt.get("status") != "queued" or not receipt.get("response", {}).get("complete"):
        raise RuntimeError(f"pane-write receipt was incomplete: {receipt_text}")
    print(receipt_text, flush=True)

    print("Send Enter as a separate literal carriage return.", flush=True)
    send_literal_enter(pane)

    # Give the TUI four seconds to move the prompt out of its composer. Retry
    # only if the prompt is still visible as draft input; a slow agent response
    # is not a reason to send another Enter into the TUI.
    time.sleep(4)
    if prompt_is_still_in_composer(pane):
        print("Prompt is still in the composer after 4 seconds; send Enter again.", flush=True)
        send_literal_enter(pane)

    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        if result_visible(pane):
            if TASK == "mail":
                print("Mail activity is visible in the private inboxes.", flush=True)
                return
            if TASK == "claim":
                print("Muse claimed its open letter.", flush=True)
                return
            lines = run("save-buffer", str(pane), "-").splitlines()
            print(f"\n{AGENT.title()} pane reply:", flush=True)
            print("\n".join(line.rstrip() for line in lines[-10:]), flush=True)
            return
        time.sleep(0.5)
    if TASK == "mail":
        raise RuntimeError("submitted prompt did not produce visible mailbox activity")
    if TASK == "claim":
        raise RuntimeError("Muse did not claim its open letter after PMUX_MAIL was submitted")
    raise RuntimeError("submitted prompt did not produce a visible PANE_WRITE_OK reply")


if __name__ == "__main__":
    demonstrate()
    if result_path := os.environ.get("PRISMATTYC_DEMO_RESULT"):
        Path(result_path).write_text("PASS\n")
