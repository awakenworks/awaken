from __future__ import annotations

import base64
import json
import re
import urllib.parse
from email import policy
from email.parser import BytesParser
from typing import Any


_BYTES_IO_REPR = re.compile(r"^<(_io\.BytesIO) object at 0x[0-9a-fA-F]+>$")


def _semantic_multipart_text(payload: bytes, charset: str) -> str:
    text = payload.decode(charset)
    # Older official SDKs accidentally emit an additional textual `files[]`
    # part containing repr(BytesIO). Its address is process-local entropy, not
    # wire meaning; retain the object kind and extra part while removing only
    # the hexadecimal identity so sync/async requests are comparable.
    return _BYTES_IO_REPR.sub(r"<\1 object>", text)


def normalized_name(name: str) -> str:
    return "".join(character for character in name.lower() if character.isalnum())


def sorted_query(request: object) -> list[list[str]]:
    pairs = urllib.parse.parse_qsl(
        request.url.query.decode(),
        keep_blank_values=True,
        strict_parsing=False,
    )
    return [list(pair) for pair in sorted(pairs)]


def semantic_headers(request: object) -> dict[str, Any]:
    betas = sorted(
        value.strip()
        for value in request.headers.get("anthropic-beta", "").split(",")
        if value.strip()
    )
    return {
        "accept": request.headers.get("accept"),
        "anthropic_beta": betas,
        "anthropic_worker_id": request.headers.get("anthropic-worker-id"),
    }


def multipart_body(content_type: str, content: bytes) -> list[dict[str, Any]]:
    boundary = next(
        (
            component.split("=", 1)[1].strip().strip('"')
            for component in content_type.split(";")
            if component.strip().startswith("boundary=")
        ),
        None,
    )
    assert boundary is not None, "Python multipart boundary is present"
    if content.strip() == f"--{boundary}--".encode():
        return []
    message = BytesParser(policy=policy.default).parsebytes(
        f"Content-Type: {content_type}\r\nMIME-Version: 1.0\r\n\r\n".encode() + content
    )
    assert message.is_multipart(), "Python multipart request is parseable"
    parts = []
    for part in message.iter_parts():
        name = part.get_param("name", header="content-disposition")
        filename = part.get_filename()
        payload = part.get_payload(decode=True)
        assert name is not None and payload is not None
        if filename is None:
            parts.append(
                {
                    "name": name,
                    "text": _semantic_multipart_text(
                        payload,
                        part.get_content_charset() or "utf-8",
                    ),
                }
            )
        else:
            parts.append(
                {
                    "content_base64": base64.b64encode(payload).decode(),
                    "filename": filename,
                    "media_type": part.get_content_type(),
                    "name": name,
                }
            )
    return sorted(
        parts,
        key=lambda value: json.dumps(value, sort_keys=True, separators=(",", ":")),
    )


def semantic_body(request: object) -> dict[str, Any]:
    content = request.content
    if not content:
        return {"kind": "empty"}
    content_type = request.headers.get("content-type", "")
    if content_type.startswith("multipart/form-data"):
        return {"kind": "multipart", "parts": multipart_body(content_type, content)}
    if content_type.startswith("application/json"):
        return {"kind": "json", "value": json.loads(content)}
    return {
        "kind": "binary",
        "content_base64": base64.b64encode(content).decode(),
    }


def semantic_request(request: object) -> dict[str, Any]:
    raw_path = request.url.raw_path.split(b"?", 1)[0].decode()
    return {
        "body": semantic_body(request),
        "headers": semantic_headers(request),
        "method": request.method,
        "path": urllib.parse.unquote(raw_path),
        "query": sorted_query(request),
    }
