#!/usr/bin/env python3
"""Refresh the model catalog snapshot from models.dev.

    python3 -I crates/qm-providers/catalog/refresh.py               # fetch https://models.dev/api.json
    python3 -I crates/qm-providers/catalog/refresh.py --input api.json  # use a saved copy

Writes `snapshot.json` next to this script: our providers' models, normalised to the fields the
app reads, sorted and pretty-printed so the diff is reviewed like any code change. Corrections
(effort levels, compatibility flags, image support) live in `corrections.toml` and are never
written here; the app overlays them when it loads the snapshot.

Standard library only. The app never fetches the catalog at runtime: updates arrive when you
rebuild.
"""

import argparse
import json
import pathlib
import re
import sys
import urllib.request

SOURCE_URL = "https://models.dev/api.json"
HERE = pathlib.Path(__file__).resolve().parent

# models.dev provider id -> our provider wire names (qm_types::domain::Provider). A subscription
# login and an API key for the same vendor share the vendor's list.
PROVIDERS = {
    "anthropic": ["claude", "anthropic"],
    "openai": ["chatgpt", "openai"],
    "google": ["gemini"],
    "xai": ["xai"],
    "openrouter": ["openrouter"],
    "github-copilot": ["copilot"],
    # Our Kimi login is the Kimi Code subscription (api.kimi.com/coding), whose list models.dev
    # keeps as "kimi-code-plan-global" (Pi's kimi-coding provider reads the same one), not
    # Moonshot's pay-per-token API ("moonshotai").
    "kimi-code-plan-global": ["kimi"],
}

# Vendor prefixes some aggregators put on model ids ("anthropic/claude-opus-5-5"); stripped to
# get the canonical id that tier placements key on.
CANONICAL_PREFIXES = ("anthropic/", "openai/", "google/", "x-ai/", "moonshotai/")


# Copilot and OpenRouter write Claude versions with a dot ("claude-opus-5.5"); Anthropic, whose
# ids the placements use, with a dash ("claude-opus-5-5"). The canonical id uses the dash, so the
# same model through two accounts shares one placement.
CLAUDE_DOTTED = re.compile(r"^(claude-[a-z]+-)(\d+)\.(\d+)$")


def canonical(model_id):
    for prefix in CANONICAL_PREFIXES:
        if model_id.startswith(prefix):
            model_id = model_id[len(prefix):]
            break
    return CLAUDE_DOTTED.sub(r"\1\2-\3", model_id)


def normalise(model_id, m):
    modalities = m.get("modalities") or {}
    inputs = modalities.get("input") or []
    limit = m.get("limit") or {}
    entry = {
        "id": model_id,
        "name": m.get("name") or model_id,
        "family": m.get("family") or None,
        "release_date": m.get("release_date") or None,
        "image_input": "image" in inputs,
        "context_window": limit.get("context") or None,
        "reasoning": bool(m.get("reasoning")),
        "tool_call": bool(m.get("tool_call")),
    }
    c = canonical(model_id)
    if c != model_id:
        entry["canonical"] = c
    return entry


def build(api):
    out = {}
    for source, ours in sorted(PROVIDERS.items()):
        provider = api.get(source)
        if provider is None:
            print(f"warning: models.dev has no provider {source!r}", file=sys.stderr)
            continue
        models = provider.get("models") or {}
        entries = [
            normalise(mid, m)
            for mid, m in sorted(models.items())
            # Crewmates need tools; a model without tool calls can never serve.
            if m.get("tool_call")
            # "~vendor/…-latest" entries are moving aliases of models listed in their own right.
            and not mid.startswith("~")
        ]
        for name in ours:
            out[name] = entries
    return {
        "source": SOURCE_URL,
        "providers": dict(sorted(out.items())),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--input", type=pathlib.Path, help="a saved copy of api.json")
    parser.add_argument("--output", type=pathlib.Path, default=HERE / "snapshot.json")
    args = parser.parse_args()
    if args.input:
        api = json.loads(args.input.read_text())
    else:
        # models.dev refuses urllib's default User-Agent (HTTP 403).
        request = urllib.request.Request(
            SOURCE_URL, headers={"User-Agent": "quartermaster-catalog-refresh"}
        )
        with urllib.request.urlopen(request, timeout=60) as r:  # noqa: S310 (fixed https URL)
            api = json.load(r)
    snapshot = build(api)
    args.output.write_text(json.dumps(snapshot, indent=2, sort_keys=True) + "\n")
    counts = ", ".join(f"{p} {len(m)}" for p, m in snapshot["providers"].items())
    print(f"wrote {args.output}: {counts}")


if __name__ == "__main__":
    main()
