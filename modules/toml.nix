# Minimal TOML renderer for `harness`'s config file.
#
# Takes Nix attrs and emits the subset of TOML 1.0 that `AppConfig`
# deserializes (see `contracts/src/config.rs`).
#
# Why not `pkgs.formats.toml`? It gets dotted keys like
# `"text.user_border"` and string/integer/boolean leaves right on its
# own, so those are not the reason. The reasons are:
#
#   * a `null` value aborts the build (`json2x`: "unsupported unit
#     type"), and dropping an optional key is exactly how a Nix caller
#     says "leave this at the built-in default" (`session.dir`,
#     `skills.dir`, ...);
#   * a `runCommand` build for what is pure string concatenation.
#
# Rules:
#   * strings: double-quoted, basic-string escapes for control chars
#   * ints / floats / bools: bare
#   * lists of scalars: inline arrays (`responses_models`, `mcp.servers.*.args`)
#   * `null`, `{}` and `[]` are omitted entirely, mirroring the
#     `skip_serializing_if` fields in `config.rs`
#   * every non-empty attrset becomes its own `[table]` section, and
#     a table's own scalars/lists are emitted before its subtables, as TOML
#     requires
{lib}: let
  # String quoting via `builtins.toJSON`: JSON `"..."` escaping matches TOML
  # basic-string escaping for everything `AppConfig` can hold (`\\`, `\"`,
  # `\n`/`\r`/`\t`, `\b`/`\f`, `\uXXXX` for other C0 controls). One gap:
  # JSON leaves DEL (U+007F) raw but TOML forbids it bare, so map it to
  # `\u007F` explicitly.
  delChar = builtins.fromJSON "\"\\u007f\"";
  fixDel = s: builtins.replaceStrings [delChar] ["\\u007F"] s;

  # `builtins.toJSON`-style quoting for string *values* (see above). Keys
  # get the same escaping only when they must be quoted.
  quoteString = v: fixDel (builtins.toJSON v);

  # TOML bare keys are `[A-Za-z0-9_-]+`. Emit those bare so the in-place
  # `model` writer (`plugins/config/src/lib.rs`) can find a literal `[llm]`
  # header and `model = …` line; quote anything else, where an unquoted key
  # would be read as a dotted path (`text.user_border` -> nested table).
  bareKeyRe = "[A-Za-z0-9_-]+";
  renderKey = k:
    if builtins.match bareKeyRe k != null
    then k
    else quoteString k;

  isScalar = v: builtins.isString v || builtins.isInt v || builtins.isFloat v || builtins.isBool v;

  isListOfScalars = v: builtins.isList v && builtins.all isScalar v;

  # Skipped rather than rendered: the caller's "leave it at the default".
  isEmpty = v:
    v
    == null
    || (builtins.isAttrs v && v == {})
    || (builtins.isList v && builtins.length v == 0);

  renderScalar = v:
    if builtins.isString v
    then quoteString v
    else if builtins.isBool v
    then
      if v
      then "true"
      else "false"
    else if builtins.isInt v || builtins.isFloat v
    then builtins.toString v
    else throw "harness: cannot render ${builtins.typeOf v} as TOML (expected string, int, float or bool)";

  renderList = v:
    if !isListOfScalars v
    then throw "harness: cannot render list as TOML (only lists of strings, ints, floats or bools are supported)"
    else "[" + lib.concatStringsSep ", " (lib.map renderScalar v) + "]";

  # `path` is the dotted key path of this table; the empty path is the
  # file itself, which has no header.
  renderTable = path: attrs: let
    entries = builtins.filter (e: !isEmpty e.value) (lib.mapAttrsToList lib.nameValuePair attrs);
    scalars = builtins.filter (e: isScalar e.value) entries;
    lists = builtins.filter (e: builtins.isList e.value) entries;
    subtables = builtins.filter (e: builtins.isAttrs e.value) entries;
    unsupported = builtins.filter (e: !(isScalar e.value || builtins.isList e.value || builtins.isAttrs e.value)) entries;
    badLists = builtins.filter (e: !isListOfScalars e.value) lists;
    ownScalars = lib.map (e: "${renderKey e.name} = ${renderScalar e.value}") scalars;
    ownLists = lib.map (e: "${renderKey e.name} = ${renderList e.value}") lists;
    own =
      if unsupported != []
      then throw "harness: cannot render `${(builtins.head unsupported).name}` as TOML (unsupported ${builtins.typeOf (builtins.head unsupported).value}; expected string, int, float, bool, list of those, or table)"
      else if badLists != []
      then throw "harness: cannot render `${(builtins.head badLists).name}` as TOML (only lists of strings, ints, floats or bools are supported)"
      else lib.concatStringsSep "\n" (ownScalars ++ ownLists);

    children = lib.map (e: renderTable (path ++ [e.name]) e.value) subtables;

    body = lib.concatStringsSep "\n\n" (lib.filter (s: s != "") ([own] ++ children));

    # A table holding only subtables needs no header of its own:
    # `[a]` immediately followed by `[a.b]` is legal but noise.
    result =
      if path == [] || (own == "" && children != [])
      then body
      else "[${lib.concatStringsSep "." (lib.map renderKey path)}]\n${body}";
  in
    result;
in
  attrs:
    if builtins.isAttrs attrs
    then renderTable [] attrs
    else throw "harness: config must be an attrsets, got ${builtins.typeOf attrs}"
