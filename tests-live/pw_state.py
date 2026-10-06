#!/usr/bin/env python3
"""Summarise the PipeWire graph state relevant to mini-eq live tests.

Stream routing targets set via the `default` metadata do NOT appear in node
props in pw-dump; they are read from the Metadata object's entries
(subject = stream id, key = target.node/target.object).

Prints one JSON object:
  {
    "sinks": {name: {"id":, "serial":}},
    "minieq": {"id":, "serial":} | None,
    "filter_output": {"id":, "target_object":} | None,
    "streams": [{"name":, "app":, "id":, "target_node":, "target_object":}],
    "analyzer": {"id":, "name":} | None,
    "monitor_links": [{"out_node":, "in_node":}]
  }
"""
import json
import os
import subprocess
import sys


def main() -> int:
    raw = subprocess.run(["pw-dump"], capture_output=True, text=True, timeout=30)
    if raw.returncode != 0:
        print(json.dumps({"error": raw.stderr.strip()}))
        return 1
    try:
        objs = json.loads(raw.stdout)
    except json.JSONDecodeError as e:
        print(json.dumps({"error": f"pw-dump not JSON: {e}"}))
        return 1

    nodes = {}  # id -> (name, serial, props)
    for o in objs:
        if not o.get("type", "").endswith("PipeWire:Interface:Node"):
            continue
        info = o.get("info", {})
        props = info.get("props", {})
        name = props.get("node.name", "")
        serial = str(props.get("object.serial", ""))
        nodes[o["id"]] = (name, serial, props)

    # Routing targets from the `default` metadata.
    targets = {}  # stream_id -> {"target.node": v, "target.object": v}
    for o in objs:
        if not o.get("type", "").endswith("PipeWire:Interface:Metadata"):
            continue
        if o.get("props", {}).get("metadata.name") != "default":
            continue
        for item in o.get("metadata", []):
            if item.get("key") in ("target.node", "target.object"):
                targets.setdefault(item.get("subject"), {})[item["key"]] = str(
                    item.get("value", "")
                )

    sinks = {}
    for nid, (name, serial, props) in nodes.items():
        if props.get("media.class") == "Audio/Sink" and "mini_eq" not in name:
            sinks[name] = {"id": nid, "serial": serial}

    minieq = None
    if os.environ.get("PW_STATE_DEFAULT_EQ"):
        want = os.environ["PW_STATE_DEFAULT_EQ"]
        for nid, (name, serial, _props) in nodes.items():
            if name == want:
                minieq = {"id": nid, "serial": serial, "name": name}
    if minieq is None:
        for nid, (name, serial, _props) in nodes.items():
            if name.startswith("mini_eq_sink"):
                minieq = {"id": nid, "serial": serial, "name": name}
                break
    # Multi-chain: every device EQ sink visible, keyed by node name.
    eq_sinks = {
        name: {"id": nid, "serial": serial, "name": name}
        for nid, (name, serial, _props) in nodes.items()
        if name.startswith("mini_eq_sink")
    }
    if env_default_eq := __import__("os").environ.get("PW_STATE_DEFAULT_EQ"):
        minieq = eq_sinks.get(env_default_eq, minieq)

    filter_output = None
    for nid, (name, _serial, _props) in nodes.items():
        if name == "mini_eq_sink_output":
            tgt = targets.get(nid, {})
            filter_output = {"id": nid, "target_object": tgt.get("target.object", "")}

    streams = []
    for nid, (name, _serial, props) in nodes.items():
        if props.get("media.class", "").startswith("Stream/Output"):
            tgt = targets.get(nid, {})
            streams.append(
                {
                    "name": props.get("node.name", props.get("app.name", "")),
                    "app": props.get("application.name", ""),
                    "id": nid,
                    "target_node": tgt.get("target.node", ""),
                    "target_object": tgt.get("target.object", ""),
                }
            )

    analyzer = None
    for nid, (name, _serial, _props) in nodes.items():
        if name == "mini-eq-analyzer" or "analyzer" in name.lower():
            analyzer = {"id": nid, "name": name}
            break

    name_by_id = {nid: n for nid, (n, _s, _p) in nodes.items()}
    monitor_links = []
    for o in objs:
        if not o.get("type", "").endswith("PipeWire:Interface:Link"):
            continue
        info = o.get("info", {})
        out_node = info.get("output-node-id")
        in_node = info.get("input-node-id")
        if analyzer is not None and in_node == analyzer["id"]:
            monitor_links.append(
                {
                    "out_node": name_by_id.get(out_node, str(out_node)),
                    "in_node": name_by_id.get(in_node, str(in_node)),
                }
            )

    print(
        json.dumps(
            {
                "sinks": sinks,
                "minieq": minieq,
                "eq_sinks": eq_sinks,
                "filter_output": filter_output,
                "streams": streams,
                "analyzer": analyzer,
                "monitor_links": monitor_links,
            }
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
