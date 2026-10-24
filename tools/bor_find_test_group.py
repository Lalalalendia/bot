#!/usr/bin/env python3
"""Resolve exactly one Telegram test supergroup on the GitHub runner.

The bot token is read only from the Actions secret environment variable;
it is never printed or passed as a command-line argument.
"""
import json
import os
import re
import sys
import urllib.error
import urllib.request


def unique_test_group(updates):
    found = set()
    for update in updates:
        if not isinstance(update, dict):
            continue
        for event in ("message", "edited_message", "my_chat_member",
                      "chat_member", "chat_join_request"):
            data = update.get(event)
            chat = data.get("chat") if isinstance(data, dict) else None
            if isinstance(chat, dict) and chat.get("type") == "supergroup":
                identifier = chat.get("id")
                if type(identifier) is int and re.fullmatch(r"-100[0-9]{5,18}", str(identifier)):
                    found.add(identifier)
    if len(found) == 0:
        raise ValueError("No test supergroup found: add the bot as admin and post /start in the test group")
    if len(found) > 1:
        raise ValueError("Multiple test groups found: set BOR_TEST_CHAT_ID GitHub Secret explicitly")
    return next(iter(found))


def discover():
    token = os.getenv("BOR_BOT_TOKEN", "")
    if not re.fullmatch(r"[0-9]{5,20}:[A-Za-z0-9_-]{20,}", token):
        raise ValueError("BOR_BOT_TOKEN GitHub Secret is missing or malformed")
    url = "https://api.telegram.org/bot" + token + "/getUpdates"
    request = urllib.request.Request(
        url, data=b'{"limit":100,"timeout":0}',
        headers={"Content-Type": "application/json"}, method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            payload = json.load(response)
    except (OSError, ValueError):
        # Exceptions can contain token-bearing URLs. Never print them.
        raise ValueError("Telegram getUpdates failed: token, webhook or concurrent polling") from None
    if not isinstance(payload, dict) or payload.get("ok") is not True:
        raise ValueError("Telegram refused getUpdates")
    if not isinstance(payload.get("result"), list):
        raise ValueError("Unexpected Telegram response")
    return unique_test_group(payload["result"])


if __name__ == "__main__":
    try:
        print(discover())
    except ValueError as exc:
        print("error=pilot_test_chat: " + str(exc), file=sys.stderr)
        sys.exit(3)
