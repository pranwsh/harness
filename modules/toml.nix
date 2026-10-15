# Minimal TOML renderer for `harness`'s config file.
#
# Takes Nix attrs and emits the subset of TOML 1.0 that `AppConfig`
# deserializes (see `contracts/src/config.rs`).
#
# Why not `pkgs.formats.toml`? It gets two things this config needs
# right on its own -- dotted keys like `"text.user_border"`, and
# string/integer/boolean leaves -- so those are not the reason. The
# reasons are:
#
#   * a `null` value aborts the build (`json2x`: "unsupported unit
#     type"), and dropping an optional key is exactly how a Nix caller
#     says "leave this at the built-in default" (`session.dir`,
#     `theme.code.name`, ...);
#   * a `runCommand` build for what is pure string concatenation.
#
# Rules:
#   * strings: double-quoted, basic-string escapes for control chars
#   * ints / bools: bare. Floats are unsupported -- `AppConfig` has
#     none. Lists are rejected too: the list-typed fields
#     (`llm.responses_models`, `mcp.servers.*.args`/`env`) are declared
#     with `types.listOf`/`attrsOf` in the module, so they never reach
#     here as Nix lists.
#   * `null`, `{}` and `[]` are omitted entirely, mirroring the
#     `skip_serializing_if` fields in `config.rs`
#   * every non-empty attrsets becomes its own `[table]` section, and
#     a table's own scalars are emitted before its subtables, as TOML
#     requires
{lib}: let
  # TOML basic-string escape for one character.
  escapeChar = ch:
    if ch == "\\"
    then "\\\\"
    else if ch == "\""
    then "\\\""
    else if ch == "\n"
    then "\\n"
    else if ch == "\r"
    then "\\r"
    else if ch == "\t"
    then "\\t"
    else if ch < " " || ch == "\u007f"
    then
      # Nix compares strings by codepoint but `lib.toHex` is byte-wise,
      # so the single-byte ASCII control characters above render as
      # two hex digits. TOML wants `\u001f`, not `\u1f`.
      "\\u" + lib.padLeft 4 "0" (lib.toHex ch)
    else ch;

  escapeString = s: lib.concatMapStrings escapeChar (lib.stringToCharacters s);

  # Always quoted, so a key means exactly what it says. Unquoted,
  # `text.user_border` would be read as a nested `text` table and the
  # whole config would fail to parse.
  quoteKey = k: "\"${escapeString k}\"";

  isScalar = v: builtins.isString v || builtins.isInt v || builtins.isBool v;

  # Skipped rather than rendered: the caller's "leave it at the default".
  isEmpty = v:
    v
    == null
    || (builtins.isAttrs v && v == {})
    || (builtins.isList v && builtins.length v == 0);

  renderScalar = v:
    if builtins.isString v
    then "\"${escapeString v}\""
    else if builtins.isBool v
    then
      if v
      then "true"
      else "false"
    else if builtins.isInt v
    then builtins.toString v
    else throw "harness: cannot render ${builtins.typeOf v} as TOML";

  # `path` is the dotted key path of this table; the empty path is the
  # file itself, which has no header.
  renderTable = path: attrs: let
    entries = builtins.filter (e: !isEmpty e.value) (lib.mapAttrsToList lib.nameValuePair attrs);
    scalars = builtins.filter (e: isScalar e.value) entries;
    subtables = builtins.filter (e: builtins.isAttrs e.value) entries;

    own =
      lib.concatStringsSep "\n"
      (lib.map (e: "${quoteKey e.name} = ${renderScalar e.value}") scalars);

    children = lib.map (e: renderTable (path ++ [e.name]) e.value) subtables;

    body = lib.concatStringsSep "\n\n" (lib.filter (s: s != "") ([own] ++ children));

    # A table holding only subtables needs no header of its own:
    # `[a]` immediately followed by `[a.b]` is legal but noise.
    result =
      if path == [] || (own == "" && children != [])
      then body
      else "[${lib.concatStringsSep "." (lib.map quoteKey path)}]\n${body}";
  in
    result;
in
  attrs:
    if builtins.isAttrs attrs
    then renderTable [] attrs
    else throw "harness: config must be an attrsets, got ${builtins.typeOf attrs}"
