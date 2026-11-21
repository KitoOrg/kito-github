# kito-github

Signing in to a [Kito](https://github.com/KitoOrg/kito) server with
GitHub. A plugin of the server: it says who somebody is, and nothing
else. Who may use which repository, and what they may do there, stays
the server's (`kito-server user add … --role …`).

```console
$ kito-server user add bob --identity github:bob --role write
bob added: whoever signs in as github:bob is bob. No token.

$ kito connect kito.example.com:7421 shop --sign-in github --fingerprint sha256:…
To sign in with github, open https://github.com/login/device
and enter the code WDJB-MJHT
Waiting until you have...
shop is connected to `shop` on kito.example.com:7421 as bob.
```

In a browser, `https://<the server>/-/sign-in/github` goes to GitHub and
comes back signed in.

## Setting it up

1. **An OAuth app at GitHub** (Settings → Developer settings → OAuth
   Apps → New OAuth App), for your account or your organization:
   - *Authorization callback URL*: `https://<the server's HTTP
     address>/-/sign-in/github/callback`. If the server has no HTTP
     address, any address will do: the command line does not use it.
   - Tick **Enable Device Flow**: that is what `kito connect --sign-in
     github` uses.
2. **The plugin on the server**:

   ```console
   $ scripts/build.sh --dist
   $ kito-server plugin add --path plugin
   $ kito-server plugin enable github
   $ kito-server github setup <client id>
   ```

   For signing in with a browser, the app's secret too, from a file so
   that it is on no command line:

   ```console
   $ kito-server github setup <client id> --secret-from secret.txt
   ```

   `kito-server github status` says what is set up. For a GitHub
   Enterprise Server, `--host https://github.example.com`.
3. **People**: `kito-server user add <name> --identity github:<login>`.
   The server keeps the number of the account, which stays when the
   login is renamed.

## What it asks of GitHub, and what it keeps

- It asks for no scope: only who the person is (their number, login,
  name, and the email they show in public).
- The token GitHub gives is used once, to ask `/user`, and kept nowhere.
- It keeps one file, `plugins/github/config.toml` under the server's
  directory, readable by the server's user only: the client id, and the
  secret if you gave one.
- It never sees a token or a session of the Kito server, and is given
  nothing of any repository (`permissions = ["sign_in",
  "server_admin"]`; `server_admin` is for `kito-server github`).
- `user add --identity` asks GitHub's public API without an account:
  sixty questions an hour from one address.

## How the server asks it

Kito runs it once for each step, with JSON on its input, and reads the
last line it prints (ADR 0006 of Kito):

| Step | Told | Answers |
| --- | --- | --- |
| `sign-in begin` | `{"kind": "device"}` | `{"open", "code", "state", "interval"}` |
| `sign-in begin` | `{"kind": "browser", "redirect"}` | `{"open", "state"}` |
| `sign-in finish` | `{"kind", "state", "query"}` | `{"identity": {…}}`, `{"wait": seconds}` or `{"refused": why}` |
| `sign-in lookup <login>` | | `{"identity": {…}}` or `{"refused": why}` |

## Development

```console
$ cargo test
```

The tests ask a GitHub that is a few lines of `tests/github.rs`; nothing
leaves the machine. The last one runs the plugin under Kito itself, and
wants `kito`, `kitod` and `kito-server` in `KITO_BIN` or in
`../kito/target/debug`.

Not tried yet with a real OAuth app: only looking a login up has been
run against GitHub itself.

## License

MIT or Apache-2.0, at your option.
