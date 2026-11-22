# kito-github

GitHub for [Kito](https://github.com/KitoOrg/kito), as a plugin. Three
things, each on its own:

- **On a team's server, a repository kept in step with one at GitHub**
  (Kito 0.39): its code both ways, and its issues as tasks. Below.

- **In a repository**: the issues at GitHub are the tasks of lanes. An
  agent reads an issue, opens its lane for it, and the issue is told,
  and closed when the lane is integrated.
- **On a team's server**: signing in with GitHub. It says who somebody
  is, and nothing else: who may use which repository, and what they may
  do there, stays the server's (`kito-server user add … --role …`).

## A repository kept in step with GitHub (on a server)

1. Enable it on the server, with pages: `kito-server plugin add github`,
   `kito-server plugin enable github`.
2. On the web client's **Plugins** page, an administrator clicks **Create
   GitHub App** (for an organization, its name): GitHub makes the
   server's app, with the right permissions and its webhook, and the
   server keeps its key. Install the app on the repositories to sync.
3. In a repository's **Settings → Plugins → GitHub**, a maintainer picks
   the repository at GitHub, the branch the main lane is, and whether
   code, issues or both follow.

Then:

- **Code, both ways**: every integration is pushed to GitHub, with the
  permanent lanes' branches; commits pushed or merged at GitHub come in
  through a lane, merged function by function and checked like any lane,
  then the merge is pushed. A real conflict waits in Kito for a decision.
- **Issues and tasks**: every open issue is a task, with what is said on
  it; a task done or dropped closes its issue, an issue closed closes its
  task; a lane of an issue's task is said on the issue.
- **When**: GitHub's webhook tells the server at once; the server also
  checks every five minutes, for when GitHub cannot reach it. **Sync
  now** in the settings does it at once.
- **No token in the repository**: git asks this plugin for one when it
  needs it (`kito-github git-credential`), an hour-long token of the
  app's installation.

## Issues as the tasks of lanes

```console
$ kito plugin enable github
$ kito github link acme/shop
$ kito github token --from token.txt      # a token that may write issues
$ kito github issues
#12 Prices lose half a cent  [bug]
$ kito lane open alice --task "acme/shop#12" --intent "prices in cents" --scope src/shop.ts:total
```

- **A lane's task names an issue**: `#12` (of the linked repository),
  `acme/shop#12`, or the issue's address.
- **The issue is told** when such a lane is opened (which lane, which
  agent, for whom, and its intent), and when it is integrated; then it
  is **closed** (`close = false` in `.kito/plugins/github/config.toml`
  leaves it open).
- **For agents**, two tools next to Kito's own: `issues` (what is open)
  and `issue` (one, with what was said on it, and the `task` to open a
  lane with). What an issue says is given as what somebody asks for,
  never as instructions that change the rules an agent works by.
- **Where it runs**: where the repository is. On a team's server, enable
  it in the repository there (`kito --repo <server>/repos/shop plugin
  enable github`): the issue is told once, by the server, whoever's
  agent opened the lane.
- **The token** is the repository's: kept in `.kito/plugins/github/
  token` for its owner only, or `KITO_GITHUB_TOKEN`. It needs to read
  and write issues of that one repository and nothing more. Without one,
  public issues are read and nothing is told.

## Signing in to a server

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
leaves the machine. The last two run the plugin under Kito itself (a
server that signs people in; a repository whose lane is for an issue),
and want `kito`, `kitod` and `kito-server` in `KITO_BIN` or in
`../kito/target/debug`.

Not tried yet against GitHub itself, but for looking a login up: neither
signing in with a real OAuth app, nor issues with a real token.

## License

MIT or Apache-2.0, at your option.
