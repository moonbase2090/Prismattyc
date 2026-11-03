# Import existing activity hosts

Endpoint import converts explicit monitor arguments or a connector hosts file
into daemon-owned endpoint definitions. Preview does not contact a daemon,
write state, enable a plug-in, grant access, or start a helper.

Preview existing monitor definitions with the same arguments:

```sh
pmux plugin endpoint import \
  --host build=/absolute/path/to/remote.sock \
  --host-cmd remote '["ssh","remote","pmux","activity","tail","--from-oldest"]'
```

Preview a held connector proposal's TOML hosts file:

```sh
pmux plugin endpoint import --activity-hosts hosts.toml
```

The file uses this format:

```toml
min_interval_ms = 1000

[[host]]
label = "local"
socket = "default"

[[host]]
label = "build"
command = ["ssh", "build", "pmux"]
```

Connector commands remain query prefixes. Monitor commands remain complete
tail commands. Import does not infer or append a different transport. The
native MCP adapter uses a 1000 ms minimum interval. Import rejects a file
with a different interval so that it cannot silently change that policy.

Review the preview, then repeat the same command with `--apply`. The selected
daemon commits the entire batch once. Reapplying identical definitions retains
their endpoint IDs. Duplicate labels, conflicting registered transports, or
more than 31 remote endpoints reject the batch before any definition changes.
Labels may contain up to 64 ASCII letters, digits, dots, underscores, or hyphens.

The `local` label is reserved for the selected daemon. An explicit matching
local socket is already represented by its local scope. Rename a conflicting
legacy host before importing it. Other `socket = "default"` entries become
explicit aliases of the selected daemon's socket.

Import grants no access. After installing a plug-in, explicitly approve the
required endpoint scopes when enabling its exact version:

```sh
pmux plugin enable org.moonbase.activity-monitor --version 1.0.0 \
  --allow activity.read:local --allow activity.read:build
```

Existing monitor arguments subsequently require an exact registered transport
and approved scope. A missing permission produces guidance; it does not start
an unmanaged helper.
