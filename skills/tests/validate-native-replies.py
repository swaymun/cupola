#!/usr/bin/env python3
"""Validate authored wonder-ui examples against the shipped catalog schema.

Requires jsonschema (scripts/live-fixtures-requirements.txt); accepts additional
raw reply Markdown paths. This does not claim rendering or skill activation.
"""
import json
from pathlib import Path
import re
import sys

from jsonschema import Draft202012Validator


ROOT = Path(__file__).resolve().parents[2]
SCHEMA = json.loads((ROOT / "packages/protocol/schemas/wonder-ui-v1.json").read_text())


def invalid_number(value):
    raise ValueError(f"{value} is not a JSON number")


def validate(path):
    source = path.read_text()
    # Column-zero complete fences are the authoring convention this skill teaches.
    blocks = re.findall(r"^```wonder-ui\n(.*?)\n```[ \t]*$", source, re.M | re.S)
    openings = len(re.findall(r"^```wonder-ui[ \t]*$", source, re.M))
    if openings != len(blocks):
        raise ValueError("unfinished wonder-ui fence")
    validator = Draft202012Validator(SCHEMA)
    for block in blocks:
        validator.validate(json.loads(block, parse_constant=invalid_number))
    print(f"{path}: {len(blocks)} native document(s) schema PASS")
    return len(blocks)


def main():
    Draft202012Validator.check_schema(SCHEMA)
    skill = ROOT / "skills/wonder-phone-replies/SKILL.md"
    count = validate(skill)
    # The isolated field example is not a document. Validate it using the
    # production form-field schema, rather than keeping a second schema here.
    source = skill.read_text()
    fields = re.findall(r"^```json\n(.*?)\n```$", source, re.M | re.S)
    form = next(component for component in SCHEMA["properties"]["components"]["items"]["oneOf"]
                if component["properties"]["type"]["const"] == "form")
    for block in fields:
        Draft202012Validator(form["properties"]["fields"]).validate(json.loads(block))
    print(f"Skill field example(s): {len(fields)} schema PASS")
    for argument in sys.argv[1:]:
        count += validate(Path(argument))
    print(f"PASS: {count} native documents, {len(fields)} field example(s)")


if __name__ == "__main__":
    main()
