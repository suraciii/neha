#!/usr/bin/env python3
"""Fetch the models.dev catalog and its license into the Space crate.

This is a manual release step. The build, the tests, and `just ci` read the
committed seed and never touch the network; run this script only when the
catalog should be updated:

    python3 scripts/update_models_dev.py

The script refuses a body that is not a non-empty provider map, and it writes
the catalog with a zero modification time so the same upstream body always
produces the same compressed bytes.
"""

from __future__ import annotations

import gzip
from http.client import HTTPException
import json
from pathlib import Path
import sys
import urllib.error
import urllib.request

API_URL = 'https://models.dev/api.json'
LICENSE_URL = 'https://raw.githubusercontent.com/sst/models.dev/dev/LICENSE'

ROOT = Path(__file__).resolve().parent.parent
DATA = ROOT / 'crates' / 'neha-space' / 'data'
CATALOG_PATH = DATA / 'models.dev.json.gz'
LICENSE_PATH = DATA / 'MODELS_DEV_LICENSE'

TIMEOUT_SECONDS = 60
USER_AGENT = 'neha-update-models-dev/1'


def fetch(url: str) -> bytes:
    """Returns the body at `url`."""
    request = urllib.request.Request(url, headers={'User-Agent': USER_AGENT})
    with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
        return response.read()


def provider_map(body: bytes) -> dict:
    """Returns the parsed catalog, refusing anything that is not a provider map."""
    try:
        catalog = json.loads(body)
    except json.JSONDecodeError as error:
        raise SystemExit(f'{API_URL} did not return JSON: {error}') from error
    if not isinstance(catalog, dict) or not catalog:
        raise SystemExit(f'{API_URL} returned no providers')
    for provider_id, provider in catalog.items():
        if not isinstance(provider, dict) or not isinstance(provider.get('models'), dict):
            raise SystemExit(f'{API_URL} returned a malformed provider {provider_id!r}')
    return catalog


def main() -> int:
    try:
        body = fetch(API_URL)
        license_text = fetch(LICENSE_URL)
    except (urllib.error.URLError, HTTPException, TimeoutError) as error:
        raise SystemExit(f'cannot fetch the catalog: {error}') from error

    catalog = provider_map(body)
    if not license_text.startswith(b'MIT License'):
        raise SystemExit(f'{LICENSE_URL} did not return the MIT license')

    DATA.mkdir(parents=True, exist_ok=True)
    compressed = gzip.compress(body, compresslevel=9, mtime=0)
    CATALOG_PATH.write_bytes(compressed)
    LICENSE_PATH.write_bytes(license_text)

    models = sum(len(provider['models']) for provider in catalog.values())
    print(f'wrote {CATALOG_PATH.relative_to(ROOT)}: {len(compressed)} bytes, '
          f'{len(catalog)} providers, {models} models')
    print(f'wrote {LICENSE_PATH.relative_to(ROOT)}: {len(license_text)} bytes')
    return 0


if __name__ == '__main__':
    sys.exit(main())
