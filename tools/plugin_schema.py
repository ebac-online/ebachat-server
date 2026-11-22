"""Generates src/plugins/schema_gen.rs from Mattermost's server/public sources (Apache-2.0): the plugin API methods
and hooks with the types of their arguments and results (plugin/client_rpc*.go), and the Go structs they carry
(field name, JSON name with ",omitempty" when the tag has it, Go type; model/ and plugin/).

    python tools/plugin_schema.py <checkout>/server/public > src/plugins/schema_gen.rs

Only the types reachable from the methods and ROOTS are written. Fields of function, channel or mutex types are left
out (gob does not send them)."""
import pathlib
import re
import sys

ROOTS = [
    "Post", "User", "Channel", "ChannelMember", "Team", "TeamMember", "Bot", "BotPatch", "Command", "CommandArgs",
    "CommandResponse", "AutocompleteData", "Status", "FileInfo", "AppError", "PluginStatus", "PluginKVSetOptions",
    "WebsocketBroadcast", "OpenDialogRequest", "MessageAttachment", "Config", "Manifest",
    "BaseMarketplacePlugin",
    # The concrete types of AutocompleteArg.Data (an interface).
    "AutocompleteTextArg", "AutocompleteStaticListArg", "AutocompleteDynamicListArg",
]
SKIP_TYPES = re.compile(r"^(func|chan)\b|i18n\.|sync\.|atomic\.|context\.|\*?sql\.|io\.|http\.")

STRUCT = re.compile(r"^type (\w+) struct \{\s*$")
ALIAS = re.compile(
    r"^type (\w+) (?:= )?([A-Z]\w*|\[\]\*?[\w.]+|map\[[\w.]+\][\w.*\[\]{}]+|string|int64|int|bool|float64)\s*$"
)
FIELD = re.compile(r"^\s*(\w+)\s+([^`/]+?)\s*(`[^`]*`)?\s*(//.*)?$")


def parse(dirpath):
    structs, aliases = {}, {}
    for f in sorted(pathlib.Path(dirpath).glob("*.go")):
        if f.name.endswith("_test.go"):
            continue
        lines = f.read_text(encoding="utf-8").splitlines()
        i = 0
        while i < len(lines):
            m = STRUCT.match(lines[i])
            a = ALIAS.match(lines[i])
            if a:
                aliases[a.group(1)] = a.group(2)
            if not m:
                i += 1
                continue
            name, fields, depth = m.group(1), [], 1
            i += 1
            while i < len(lines) and depth > 0:
                line = lines[i]
                depth += line.count("{") - line.count("}")
                if depth == 1 and line.strip() and not line.strip().startswith("//"):
                    fm = FIELD.match(line)
                    if fm and fm.group(1)[0].isupper() and not fm.group(2).startswith("struct"):
                        go, ty, tag = fm.group(1), fm.group(2).strip(), fm.group(3) or ""
                        # The JSON name, with ",omitempty" when the tag has it.
                        jm = re.search(r'json:"([^"]*)"', tag)
                        jname, _, opts = (jm.group(1) if jm else "").partition(",")
                        js = jname or go
                        if js != "-" and "omitempty" in opts.split(","):
                            js += ",omitempty"
                        if not SKIP_TYPES.search(ty):
                            fields.append((go, js, ty.replace("model.", "")))
                i += 1
            structs[name] = fields
    return structs, aliases


Z_STRUCT = re.compile(r"^type Z_(\w+) struct ?\{(\})?\s*$")
Z_FIELD = re.compile(r"^\s*(\w+)\s+(\S+)")
CLIENT = re.compile(r"^func \(g \*(apiRPCClient|hooksRPCClient)\) ([A-Z]\w*)\(")


def rpc_methods(plugin_dir):
    """{"api"|"hooks": [(rpc method, [(field, type)] args, [(field, type)] returns)]} from the RPC clients."""
    zs, bodies = {}, []
    for name in ("client_rpc_generated.go", "client_rpc.go"):
        lines = (pathlib.Path(plugin_dir) / name).read_text(encoding="utf-8").splitlines()
        i = 0
        while i < len(lines):
            m = Z_STRUCT.match(lines[i])
            c = CLIENT.match(lines[i])
            if m:
                fields = []
                if not m.group(2):
                    i += 1
                    while i < len(lines) and lines[i].strip() != "}":
                        f = Z_FIELD.match(lines[i])
                        if f and not SKIP_TYPES.search(f.group(2)):
                            fields.append((f.group(1), f.group(2).replace("model.", "")))
                        i += 1
                zs[m.group(1)] = fields
            elif c:
                body = []
                i += 1
                while i < len(lines) and lines[i] != "}":
                    body.append(lines[i])
                    i += 1
                bodies.append((c.group(1), chr(10).join(body)))
            i += 1
    out = {"api": [], "hooks": []}
    for kind, body in bodies:
        call = re.search(r'Call\("Plugin\.(\w+)"', body)
        args = re.search(r"Z_(\w+Args)\b", body)
        rets = re.search(r"Z_(\w+Returns)\b", body)
        if not (call and args and rets) or args.group(1) not in zs:
            continue
        entry = (call.group(1), zs[args.group(1)], zs.get(rets.group(1), []))
        key = "api" if kind == "apiRPCClient" else "hooks"
        if all(e[0] != entry[0] for e in out[key]):
            out[key].append(entry)
    return out


def named(ty):
    """The struct and alias names a Go type expression refers to."""
    return re.findall(r"[A-Z]\w*", re.sub(r"\b(string|int|int8|int16|int32|int64|uint\w*|bool|float\w*|byte|any|interface)\b", "", ty))


def main():
    public = pathlib.Path(sys.argv[1])
    structs, aliases = parse(public / "model")
    pstructs, paliases = parse(public / "plugin")
    for k, v in pstructs.items():
        structs.setdefault(k, v)
    for k, v in paliases.items():
        aliases.setdefault(k, v)
    methods = rpc_methods(public / "plugin")
    keep, todo = [], list(ROOTS)
    for entries in methods.values():
        for _, args, rets in entries:
            for _, ty in args + rets:
                todo.extend(named(ty))
    while todo:
        t = todo.pop(0)
        if t in keep:
            continue
        if t in structs:
            keep.append(t)
            for _, _, ty in structs[t]:
                todo.extend(named(ty))
        elif t in aliases:
            keep.append(t)
            todo.extend(named(aliases[t]))
    out = [
        "// Generated by tools/plugin_schema.py from Mattermost v12.1.0 server/public, Copyright (c) 2015-present",
        "// Mattermost, Inc., Apache-2.0. Do not edit by hand.",
        "",
    ]
    for key, doc in (("api", "Plugin API methods"), ("hooks", "Hooks")):
        out += [
            f"/// {doc}: (RPC method, arguments, results), each a list of (field, Go type).",
            f"#[rustfmt::skip]",
            f"pub static {key.upper()}: Methods = &[",
        ]
        for name, args, rets in methods[key]:
            a = ", ".join(f'("{f}", "{t}")' for f, t in args)
            r = ", ".join(f'("{f}", "{t}")' for f, t in rets)
            out.append(f'    ("{name}", &[{a}], &[{r}]),')
        out += ["];", ""]
    out += [
        "pub type Methods = &'static [(&'static str, Fields, Fields)];",
        "type Fields = &'static [(&'static str, &'static str)];",
        "pub type StructFields = &'static [(&'static str, &'static str, &'static str)];",
        "",
        "/// Structs by name: (Go field, JSON name, Go type).",
        "#[rustfmt::skip]",
        "pub static STRUCTS: &[(&str, StructFields)] = &[",
    ]
    for t in keep:
        if t in structs:
            fields = ", ".join(f'("{g}", "{j}", "{ty}")' for g, j, ty in structs[t])
            out.append(f'    ("{t}", &[{fields}]),')
    out += ["];", "", "/// Named types that are not structs.", "#[rustfmt::skip]", "pub static ALIASES: &[(&str, &str)] = &["]
    for t in keep:
        if t in aliases and t not in structs:
            out.append(f'    ("{t}", "{aliases[t]}"),')
    out.append("];")
    sys.stdout.buffer.write(("\n".join(out) + "\n").encode())


if __name__ == "__main__":
    main()
