# Configuration

Every value that changes how Kairos behaves, in one place. Kairos 0.10.0.

Configuration has four surfaces, and they are layered rather than alternative:
the server reads **environment variables** only; the **Helm chart** and the
**reference Compose deployment** are two ways of setting those variables; the
**CLI** has a small configuration surface of its own, unrelated to the
server's.

## Server environment variables

Sixteen variables are read once at startup. For those sixteen: a required
variable that is unset, or any variable set to an unparseable value, fails
startup with a message naming the variable — invalid values are never silently
replaced by defaults — and an empty or whitespace-only value is treated as
unset.

The five retention variables are not among them. The server reads them at
startup too, but their parsing rules are different. That section gives them.

### Required

| Variable | Type | Description |
|---|---|---|
| `DATABASE_URL` | Postgres connection URL | The external PostgreSQL, which is the deployment's sole state. Example: `postgres://kairos:kairos@localhost:41432/kairos`. Also read directly by every server subcommand, including `migrate`, before the rest of the configuration is resolved. `sslmode` can be `disable`, `prefer` (the default) or `require`. With `prefer` or `require`, every connection uses TLS when the server offers it, and Kairos does not verify the certificate of the server. `verify-ca`, `verify-full` and `sslrootcert` are not accepted. |
| `OIDC_ISSUER_URL` | URL | The OIDC issuer. Discovery and JWKS endpoints are derived from it. A trailing slash is stripped. **Not required when `KAIROS_LOCAL_AUTH` is on** — see below. |
| `OIDC_AUDIENCE` | string, or comma-separated list | The `aud` claim bearer tokens must carry. A comma-separated allow-list is accepted for issuers that mint a distinct `aud` per OAuth client, such as Google Workspace; a token matching any listed audience validates. Enforced non-empty at startup. Required exactly when `OIDC_ISSUER_URL` is set. |

#### Running with no identity provider

The two OIDC variables are required **unless `KAIROS_LOCAL_AUTH` is on**, in which
case a deployment can run with no identity provider at all: people log in with a
password and nothing else is needed. That is the point of the exception, and it is what
makes Kairos runnable by a small team or on a laptop without first standing up Dex or a
cloud OAuth client.

The condition is *local auth is on*, not *the variable is empty*. A deployment that
means to use OIDC and mistyped its issuer still fails at startup with the message it
always gave — coming up with no way for anyone to log in would be a far worse failure
than refusing to start. For the same reason, setting one of the pair without the other
is refused: the symptom of a missing audience is every token being rejected, which
points at the token rather than at the configuration.

With no issuer, a JWT bearer gets a 401 that **says** there is no issuer configured.
That is the one failure on the auth path that deliberately names its cause: everywhere
else a uniform message protects a secret, and here there is none — no amount of
guessing turns "this server has no issuer" into access, and a caller told only
"invalid token" would search their own token for a fault that is not there.

`GET /api/config` reports `issuer: null` and `local_auth: true`, so the GUI offers a
password form and no SSO button it cannot honour. `kairos login` without `--email`
says that the deployment has no issuer and uses local accounts. For CLI access on such
a deployment, use [`kairos login --email`](cli.md#kairos-login).

### Network and logging

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_BIND_ADDR` | socket address | `127.0.0.1:8080` | Where the HTTP server listens. Rejected at startup if it does not parse as a socket address. Both packaged deployments override it so the container is reachable from outside: the Helm chart to `0.0.0.0:<containerPort>`, the Compose file to `0.0.0.0:8080`. |
| `KAIROS_LOG_LEVEL` | tracing filter directive | `info` | Passed to the tracing filter; accepts per-target directives, e.g. `info,kairos_server=debug`. |
| `KAIROS_LOG_FORMAT` | `json` \| `pretty` | `json` | Log encoding. Any other value fails startup. |
| `KAIROS_PUBLIC_URL` | URL | unset | The deployment's externally reachable base URL. Required to render the webhook delivery URL an operator pastes into a forge. A trailing slash is stripped. Not inferred from the request `Host` header. |

### Local password accounts

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_LOCAL_AUTH` | bool | `false` | Accept local password accounts in addition to the OIDC issuer. Off means `POST /api/login` is **not routed** — a 404 from an absent route, not a 401 from a handler that declines. |
| `KAIROS_SESSION_TTL_SECS` | integer | `1209600` (14 days) | How long a session bearer lasts. |
| `KAIROS_BOOTSTRAP_ADMIN` | email | unset | A first-boot admin, created **only on a boot that finds no users at all**. See below. |
| `KAIROS_BOOTSTRAP_PASSWORD` | string | unset | Its password. Prefer the hash form. |
| `KAIROS_BOOTSTRAP_PASSWORD_HASH` | PHC string | unset | Its password, already hashed — produce one with `kairos-server hash-password`. Wins over the plaintext when both are set. |

#### The first boot

A fresh deployment with local auth has nobody who can log in and no way to create
anybody, so `KAIROS_BOOTSTRAP_ADMIN` makes the first account.

The obvious shape has a trap worth designing around rather than discovering: **an
environment variable in a values file is a permanent credential**, in the deployment
manifest and in the release history. So the mechanism is **single-use**. It is consumed
on a boot that finds `public.users` empty, and inert on every boot after — including a
boot with the same email, which will *not* reset the password of the account it made.
An idempotent bootstrap would silently restore a known password on every restart, which
is a backdoor with a documented name.

Every outcome is logged, and the two that are not the happy path are logged at WARN,
because each is something to act on. When it is consumed, the log says to remove the
variables. When it is inert, the log says the same and adds why — silence there is how a
bootstrap password survives in a values file for a year.

The email also becomes a **deployment admin** (as `local:<email>`, the synthetic
`external_id` a local account gets). That is not a convenience: a fresh deployment has
no organization either, and creating one is a deployment-admin action, so without it the
bootstrap admin can log in and do nothing at all — which reads as a broken install
rather than a missing variable.

Prefer `KAIROS_BOOTSTRAP_PASSWORD_HASH`. `kairos-server hash-password` prints a PHC
string and nothing else, needs no database, and can be run before the deployment exists,
so the plaintext never has to be written into a file. In the Helm chart, set it in a
values file rather than with `--set`: a PHC string contains commas, and `--set` would
silently truncate it to something that parses as a hash and then never matches the
password.

#### Recovering an account

There is no password-reset email. Two paths exist instead:

- **An organization admin** resets it: `PUT /api/local-accounts/{user_id}/password`.
- **An operator** resets it with no login at all:
  `kairos-server set-password --email <email>`, which reads the password from stdin when
  `--password` is absent. It lives beside `drop-tenant` among the
  [operator subcommands](cli.md#operator-subcommands-kairos-server), because the case it
  exists for is that nobody can log in — so requiring a login is precisely what it cannot
  do. It refuses to *create* an account, since that would be a way to mint an admin on
  any deployment whose database you can reach.

Either way, **setting a password revokes every session that person held**. A password
change after a suspected compromise that left the attacker logged in would defeat the
only thing the person was trying to do.

Passwords must be at least **12 characters**, with no composition rules. A length floor
reliably buys entropy; "one upper, one digit, one symbol" mostly buys `Password1!` and
makes people write the password down.

Local accounts are **additive**, not an alternative: a deployment may have both an
issuer and local accounts, and neither path knows about the other. A person with one
email address is one `users` row either way (KAIROS-T-0197).

Accounts are created by an organization admin. There is no self-service sign-up and no
password-reset email — the two intended uses are a small team with no identity
provider, and a break-glass admin for when an issuer is unreachable.

A successful login returns an opaque bearer, `kairos_ss_<64-hex>`, which the client
presents as `Authorization: Bearer <token>`. Only its SHA-256 is stored, so a
database dump yields nothing usable. Unlike a service-account API key it carries no
tenant: a session stands in for an OIDC token, and an OIDC token is deployment-wide,
so one login covers every organization the person belongs to and membership is
enforced per request as usual.

Two weeks is the default lifetime because it is a decision and not an accident.
Forever would be a permanent credential handed out by a login form; an hour logs
people out in the middle of the work they came to do.

`POST /api/logout` revokes the presented session. **Changing a password revokes
every other session that person holds** — enforced in the storage layer, in the same
transaction as the password write, because a password change after a suspected
compromise that left the attacker logged in would defeat the only thing the person
was trying to do.

Every way a login can fail — unknown email, wrong password, an account that has no
password because it authenticates through the issuer, a revoked or expired session —
returns one 401 with one message, and an unknown email deliberately costs the same
argon2 work as a real attempt. Otherwise the response time would answer the question
the message refuses to. Failed logins are throttled per account and per source; see
[Failed-authentication throttling](#failed-authentication-throttling).

### Failed-authentication throttling

Repeated failed authentications are throttled (KAIROS-T-0202). This matters most
for the local password login (KAIROS-I-0018): an OIDC token and an API key are
both high-entropy random values that nobody guesses, but a password is guessable
by definition.

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_AUTH_MAX_FAILURES` | integer | `5` | Failures inside the window before a lockout. **`0` turns throttling off** — the escape hatch for a throttle that is misfiring, so that the fix does not need a new build. |
| `KAIROS_AUTH_FAILURE_WINDOW_SECS` | integer | `300` | How long failures accumulate. A failure older than this is forgotten, so someone who mistypes once a week never accumulates a lockout. |
| `KAIROS_AUTH_LOCKOUT_SECS` | integer | `60` | How long a lockout lasts. Short on purpose — see below. |
| `KAIROS_TRUSTED_PROXY` | bool | `false` | Whether `X-Forwarded-For` is trusted for the client address. See below. It also lets the server read `X-Forwarded-Proto` for HSTS (see "Security headers"). |

Two things are counted separately: the **identity** being attempted, and the
**source** it came from. Neither alone is enough. Counting only identities lets an
attacker try one password each against a thousand accounts; counting only sources
is meaningless behind a proxy, where every request shares one address.

The lockout is deliberately short. A long one is a denial-of-service an attacker
can aim at an account they know the name of, and it turns one person's typo into a
support request. Five more failures locks the subject out again, so a script gains
nothing from the brevity.

#### What is trusted for the client address

`X-Forwarded-For` is read **only** when `KAIROS_TRUSTED_PROXY` is on, and it is
off by default. The header is caller-supplied: anyone can send one. On a directly
exposed server, trusting it gives an attacker a new source per request, which does
not weaken a source-based throttle so much as remove it.

Turn it on only when **every** request reaches Kairos through a proxy you control
that appends the peer it saw. The reference compose stack qualifies and sets it
itself — the Kairos container publishes no port, so Caddy is the only way in. A
server you expose directly does not qualify. The chart follows `ingress.enabled`
unless you say otherwise, because behind an Ingress the socket peer is the
ingress controller and the header is the only real client address there is.

When the header is trusted, the **last** value in the list is used, not the first.
A proxy appends, so the list reads `<whatever the client sent>, <what the proxy
saw>`; the first element is the one under the caller's control.

### API keys and agent keys

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_API_KEY_TTL_DAYS` | integer | `90` | The default life of a new API key or agent key, in days. A request that names `expires_at` sets its own expiry. A request with `no_expiry: true` (the CLI flag `--no-expiry`, the switch "No expiry" of the Agent keys page) makes a key with no expiry. `0` turns the default off: a new key has no expiry unless the request names one. |
| `KAIROS_API_KEY_ROTATION_GRACE_SECS` | integer | `86400` (1 day) | After a rotation of a key, the old secret works for this time. `0` ends the old secret at the rotation. |

The keys that exist keep their expiry: the setting applies to new keys. See
[Rotate keys](../how-to/rotate-keys.md).

### Security headers

The server sends these headers on each response:

| Header | Value |
|---|---|
| `X-Content-Type-Options` | `nosniff` |
| `X-Frame-Options` | `DENY` |
| `Referrer-Policy` | `same-origin` |
| `Content-Security-Policy` | On the page of the GUI: `default-src 'self'`, scripts from the server with the hash of each inline script of the page and `'wasm-unsafe-eval'`, inline styles, images from the server, `data:`, `blob:` and HTTPS sites, fonts from the server and `fonts.gstatic.com`, `object-src 'none'`, `frame-ancestors 'none'`. On each other response: `frame-ancestors 'none'`. |
| `Strict-Transport-Security` | `max-age=31536000; includeSubDomains`, only when `KAIROS_TRUSTED_PROXY` is on and the proxy sends `X-Forwarded-Proto: https`. |

The server speaks HTTP; a proxy in front of it ends TLS. So the server sends HSTS
only when a trusted proxy says that the request came over HTTPS. The reference
compose stack (Caddy) does. No setting turns the headers off.

#### Where the state lives

In process, in memory. The consequences are worth knowing:

- Lockouts are **per replica**. A Deployment scaled to three pods, or behind the
  chart's HPA, allows roughly three times the configured failures overall.
- A restart forgets every lockout.

This is the right trade for the single-binary deployments Kairos targets
(KAIROS-A-0013): the alternative puts a database write on the failure path of an
endpoint that is under attack, which is the moment you least want extra writes.
If you run many replicas and need a shared limit, rate-limit at the ingress.

A lockout increments `kairos_auth_lockouts_total{subject="identity"|"source"}` on
`/metrics` and logs at WARN. The subject *kind* is recorded; the email address or
IP is not, because a log line outlives the incident it was gathered for.

### Trace export

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_OTEL_ENDPOINT` | URL | unset | The OTLP/**HTTP** traces endpoint, e.g. `http://collector:4318/v1/traces`. **Unset disables tracing entirely** — no exporter is built and no span leaves the process. A URL that cannot be reached does not stop Kairos serving; it logs `otel: tracing is DISABLED` and carries on. |
| `KAIROS_OTEL_SAMPLE_RATIO` | 0.0–1.0 | `1.0` | Head sampling. Refused at startup if it does not parse or falls outside the range, rather than clamped — a clamped typo produces a collector that is mysteriously empty. An empty value means unset. |

Kairos exports over **HTTP/protobuf, not gRPC**, so the port is `4318` and the
path is part of the endpoint. A collector's gRPC port (`4317`) will accept the
connection and then reject the payload, which is a slow way to discover this.

Sampling is head-only: the decision is made when a trace starts, and it respects
a decision an upstream caller already made rather than cutting a trace in half.
Keeping only the slow and failed traces is *tail* sampling, which belongs in your
collector — it can see a whole trace, and this process cannot.

One span per HTTP request is produced, named `<METHOD> <matched route>` — the
route **pattern**, never the concrete path, so a collector groups requests by
operation instead of showing one operation per id. It carries the method, route,
status code and tenant, and is marked as an error only for 5xx: a 404 or a 403 is
the server working correctly.

### Tenant resolution

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_BASE_DOMAIN` | domain | unset | Enables Host-subdomain tenant resolution: `acme.<base>` resolves to tenant `acme`. |
| `KAIROS_SINGLE_TENANT` | tenant slug | unset | Pins one tenant and skips subdomain and header resolution entirely. |
| `KAIROS_SINGLE_TENANT_NAME` | string | the slug | The name of the pinned tenant. When that tenant does not exist, the first request from a principal in `KAIROS_DEPLOYMENT_ADMINS` provisions it with this name and makes that principal its first admin. A request from a different principal gets 404 `TENANT_NOT_FOUND`. |
| `KAIROS_AUTO_JOIN_DOMAINS` | comma-separated email domains | empty | A person whose issuer verifies an email at one of these domains (`email_verified: true`) becomes a `member` of the pinned tenant on the first request, with no admin step. The match is exact, so `example.com` does not match `eu.example.com`. A person that an admin or SCIM removed from the organization does not join again, and an admin can add them back. Requires `KAIROS_SINGLE_TENANT`: the server does not start when it is set without that. An entry with an `@` or with no dot also stops the start. |

Exactly one of the two applies. With neither set, only the `X-Tenant` header
resolves a tenant.

The two packaged deployments treat this differently. The Helm chart requires
exactly one: rendering fails when both are set and also when neither is. The
Compose file enforces nothing — it forwards both variables unconditionally, and
the only guard is a comment in `deploy/.env.example`.

### Identity and the browser client

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_WEB_CLIENT_ID` | string | `kairos-web` | The public OAuth client id the GUI uses for its PKCE flow. |
| `KAIROS_API_BEARER` | `access_token` \| `id_token` | `access_token` | Which OIDC token the GUI and CLI send as the `/api` bearer. Any other value fails startup. Server-side validation is identical either way — it validates whatever RS256 JWT arrives — so this only tells the clients which token to send. `id_token` is for issuers whose access token is opaque and therefore unvalidatable, notably Google and Google Workspace. |
| `KAIROS_WEB_CLIENT_SECRET` | string | unset | The OAuth client secret for a confidential GUI client. Unset keeps public-client behaviour, which suits Dex and Keycloak. When set, the server-side token relay presents it on the code and refresh exchanges. Required for Google Workspace. Server-side only: it is never sent to the browser and never appears in `/api/config`. |
| `KAIROS_DEPLOYMENT_ADMINS` | comma-separated OIDC `sub`s | empty | Principals — human users or service accounts — allowed to call the cross-tenant `/api/admin/tenants` routes. Entries are trimmed and empty entries dropped. Empty or unset means those routes always return 403. |

### Forge webhooks

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_WEBHOOK_SIGNING_KEY` | string | unset | The deployment secret every per-connection webhook secret is derived from. When unset, forge connections can be neither created nor verified: the feature is off rather than degraded. |

Neither `KAIROS_WEBHOOK_SIGNING_KEY` nor `KAIROS_PUBLIC_URL` has a dedicated
Helm value or `.env.example` entry; both are set through the chart's
`extraEnv` or the container environment directly.

### The base code index

Kairos keeps the code index of each indexed commit of a repository
(`/api/repositories/{slug}/code-indexes`). The indexes are in Postgres. These
variables control the clones of the repositories and the builder. The builder
makes the first index of each repository that has no index. Then it updates
the index after each push to the default branch.

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_CODE_INDEX_DIR` | directory path | unset | The folder for a bare clone of each repository, the tools of the builder and the work folders of the builder. Put it on a volume. When it is not set, the server cannot find the nearest indexed commit, does not build and downloads nothing. Uploads and downloads of indexes still work. |
| `KAIROS_CODE_INDEX_POLL_SECS` | whole number | `300` | How often the sweep fetches each indexed repository and adds the build jobs. The sweep is a periodic job: one server runs it at each interval. `0` turns the builder off on that server. |
| `KAIROS_CODE_INDEX_THREADS` | whole number, 1 or more | `4` | The number of CPU threads that the summary model of the builder uses. More threads make the summaries faster, but they take CPU from the other services on the host. `0` or a value that is not a number stops the start of the server. |
| `KAIROS_CODE_INDEX_MIN_REBUILD_SECS` | whole number | `0` | The least time in seconds between 2 updates of one repository. `0` turns the limit off. On a slow host, a repository with many pushes then waits, and the other stale repositories get their updates. A value that is not a number stops the start of the server. |

#### The providers of the summaries and the vectors

Each organization chooses where the summaries of its code index are made.
It also chooses where the vectors of those summaries are made. The setting
is for the organization, not for the deployment. An organization admin sets it on the
page Admin, Code index, or with `kairos admin code-index-settings set`. The
REST route is `PUT /api/org/code-index-settings`. Each member can read it.

| Setting | Values | Default | Description |
|---|---|---|---|
| `summary.provider` | `embedded`, `ollama-cloud`, `bedrock` | `embedded` | `embedded` is the model in the server. `ollama-cloud` is an OpenAI-compatible chat endpoint: it also serves a local Ollama, vLLM and OpenRouter. `bedrock` is AWS Bedrock. |
| `summary.base_url` | URL | none | The base URL of the endpoint, for `ollama-cloud`. For example `https://ollama.com/v1`. |
| `summary.model` | text | none | The model name (`ollama-cloud`) or the model id (`bedrock`). |
| `summary.region` | text | none | The AWS region, for `bedrock`. |
| `summary.secret` | text | none | The API key (`ollama-cloud`), or the AWS credentials as `<access key id>:<secret access key>` with an optional `:<session token>` (`bedrock`). |
| `vectors.provider` | `embedded`, `remote` | `embedded` | `remote` is an OpenAI-compatible embeddings endpoint. |
| `vectors.base_url` | URL | none | The base URL of the embeddings endpoint, for `remote`. |
| `vectors.model` | text | none | The model name of the embeddings, for `remote`. |
| `vectors.secret` | text | none | The API key of the embeddings endpoint. A local Ollama needs none. |
| `concurrency` | 1 to 32 | `4` | The requests that a hosted summarizer sends at a time. |
| `default_summaries` | `embedded`, `hosted` | `embedded` | The summarizer of each repository that follows the organization. `hosted` needs a hosted `summary.provider`. A write that does not give it keeps the stored value. |

Kairos keeps each secret encrypted with `KAIROS_SECRETS_KEY`, as it keeps
the read tokens of the repositories. No read gives a secret back: a read
says that it is set, by whom and when. A write that does not name a secret
keeps the stored secret. An empty secret removes it. A hosted provider
needs its secret, its URL or region, and its model. Kairos refuses a write
with one of them missing, and the refusal names the field.

`code_search` of the code tools compares the vector of its query with the
vectors of the index. One model must make the two vectors. A CLI with no
local model of the index asks Kairos for the vector: `POST
/api/code-index/query-vector`. Kairos uses its embedded model, or the remote
provider of the organization with its secret.

Each member can ask. The text has 1 to 2,000 characters. When the
organization has no provider for the model, Kairos refuses the request
(`NO_QUERY_PROVIDER`). The search then uses the text of the summaries.

The setting `code_index_summaries` of a repository chooses its summarizer:

- `organization` (the default of a new repository): the `default_summaries`
  of the organization decides.
- `embedded`: the embedded model, whatever the organization sets. The code
  stays on the host.
- `hosted`: the hosted provider of the organization.

A person sets it on the page Admin, Repositories. The command is `kairos
repos update <slug> --code-index-summaries <value>`. Kairos refuses `hosted`
when the organization has no hosted provider. A repository from before
0.9.0 keeps the value that it had.

When the summarizer is the embedded model, the code of the repository stays
on the host.

For a repository on a hosted provider, the builder sends `concurrency`
requests at a time. After a 429 or a 5xx, it tries the request again, 3
tries in all. A hosted build does not hold the CPU of the host for its
summaries. So it does not wait for the builds of the embedded model.

Each run records the model that wrote its summaries. See it with `kairos
repos builds <slug>`, or in the panel of the repository. The image
`ghcr.io/colliery-io/kairos:<version>-hosted` has no embedded model and
downloads none. It builds only the repositories on a hosted provider. The
other repositories get a failed run that says so. See [Use a hosted model
for the code index summaries](../how-to/use-a-hosted-model-for-the-code-index.md).

The clone fetches from the `repo_url` of the repository. For a private
repository, set a read token on the repository (see
[Read a private repository](../how-to/read-a-private-repository.md)). With no
token, the clone has no credential, and a fetch that needs one fails at once.

#### The work of the builder

Each build is a job of the [job queue](#job-queue). Thus 2 or more servers
share the builds, and each build runs one time. There are 3 types of build:

- **A requested build.** A person asks for it (`kairos repos reindex`, or
  the button "Build again" of the panel of the repository). It is a full index of the head
  of the default branch.
- **An update.** At each interval of `KAIROS_CODE_INDEX_POLL_SECS`, the
  sweep fetches each repository that has an index. If the head of the
  default branch has no index, the sweep adds an update job. The update
  changes the nearest index to that head. The sweep is a periodic job (see
  [Job queue](#job-queue)): at each interval, one server runs it.
- **A first build.** At the same interval, the sweep adds a job for each
  repository that has no index. The job makes a full index of the head of
  the default branch. The index has the structure, the SCIP edges and the
  summaries.
  A first build of a large repository can take some hours on a CPU.

A repository has one build job at a time. A server does not add a second
job for a repository that has a job. The note of the pass names that job.

The jobs run in this order: a requested build, then an update, then a first
build. Inside one type, the repository that has been stale longest goes
first. The order of the list and the time of the pushes do not change it. A
repository that got an update goes to the end of the next pass. A repository
with many pushes does not get 2 updates in a row while another repository is
stale.

With `KAIROS_CODE_INDEX_MIN_REBUILD_SECS`, a repository whose newest index
is younger than that time gets no update job in the pass. A later pass adds
the job.

A build with the embedded model is a job of the type `embedded_build`. One
server runs one of them at a time (`KAIROS_JOB_LIMIT_EMBEDDED_BUILD`). Thus
the builds use the `KAIROS_CODE_INDEX_THREADS` threads one time, not 2
times. A build with a hosted provider is a job of the type `hosted_build`.
One server runs 4 of them at a time (`KAIROS_JOB_LIMIT_HOSTED_BUILD`).

A first build gives its place to an update or a requested build of the same
type. The first build stops after its next summary, and goes back to the
queue. It keeps its summaries and its run. Thus, during a long first build,
the updates continue.

A build tells the job queue about its progress at each summary and at each
batch of vectors. When a build makes no progress for the lease time, its
server stops it. Then a server starts it again. When a server stops during a
build, another server takes the job when the lease runs out.

A first build keeps its work in `KAIROS_CODE_INDEX_DIR/first-builds/`. An
update keeps its work in `KAIROS_CODE_INDEX_DIR/updates/`. Each summary goes
into that file when the model writes it. The next build of the repository
uses those summaries again. Thus a restart during a build of some hours does
not lose the work of the model. When the servers do not share
`KAIROS_CODE_INDEX_DIR`, a build that another server takes starts its
summaries again.

After the builder stores the index, it removes the file. An index keeps only the
summaries that its code needs. The run shows the number of summaries that it
made and the number that it reused.

If a first build of a commit fails, the server does not try that commit
again. It tries again when the head moves or when the server starts again.

Each run records the server that runs it. When a server takes the job of a
run again, the run continues on that server. When a server starts, its
builder ends each run that no open job names. Such a run gets the text "The
server stopped during the run". When a job fails after its last try, its run
fails with the reason.

The log has these lines for each build:

- `code index full build started`, with the repository, the commit and the
  type.
- `code index built`, with the time in seconds and the number of summaries.
- `code index not built`, with the error.
- `code index build not done`, with the reason. For example, the head has an
  index, or the repository has `code_index_build` off. The builder logs a
  reason one time, and again only when the reason changes.

You can also make the first index on a computer and upload it
(`kairos index build`, then the upload). Then the builder does not make a
first build of the repository.

#### Stop the builder for one repository

Each repository has the setting `code_index_build`:

- `on` (the default): the builder makes the first index and the updates.
- `off`: the builder does nothing for the repository. Use it for a template
  or a static site. An upload of an index still works.

Kairos refuses a value that is not `on` or `off`, and the error names the
value. To change the setting, use one of these:

- `kairos repos update <slug> --code-index-build off`
- `PATCH /api/repositories/{slug}` with `{"code_index_build": "off"}`
- the **Edit** form of the repository on the admin **Repositories** page

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_SECRETS_KEY` | 32 bytes in base64 | unset | The key that encrypts the read token of each repository (AES-256-GCM). Make it with `openssl rand -base64 32`. When it is not set, the server refuses a token with 501 `SECRETS_NOT_CONFIGURED`, and the builder reads public repositories only. A value that is not 32 bytes of base64 stops the start of the server. Keep the key outside the database, with the database password. |

| `KAIROS_SECRETS_KEY_OLD` | base64 keys, separated by commas | unset | The old keys, during a rotation of `KAIROS_SECRETS_KEY`. They decrypt the rows that they encrypted; they never encrypt. Set it only with `KAIROS_SECRETS_KEY`. |

To change `KAIROS_SECRETS_KEY`, put the new key in `KAIROS_SECRETS_KEY` and the
old key in `KAIROS_SECRETS_KEY_OLD`, then run `kairos-server rotate-secrets`. It
encrypts each stored secret again with the new key: the read token of each
repository, and the secrets of the hosted models of the code index. See
[Rotate keys](../how-to/rotate-keys.md). A stored secret that no key of the two
settings decrypts fails with the error "Set the token again". Set that secret
again.

#### The tools of the builder

The image does not contain the tools of the builder. The builder needs these
conditions:

- `KAIROS_CODE_INDEX_DIR` is set.
- `KAIROS_CODE_INDEX_POLL_SECS` is not `0`.
- Embeddings are on.
- The server has the `llama` feature.

Then, at each start, the server checks the tools in
`KAIROS_CODE_INDEX_DIR/tools/`:

- It downloads each file that is not there (about 2.7 GB on the first
  start). The download runs in the background. The server serves during the
  download.
- It checks each file against its pinned sha256. It does not download a file
  again when the file has the correct checksum.
- After a check, it records the size and the modification time of each file
  in `tools/.kairos-index-checked`. At the next start, it reads a file again
  only if its size, its modification time or its pin changed. Then it reads
  the full file and checks it again. Thus a start
  after the first start does not read the 2.7 GB of files. To check all the
  files again, remove `tools/.kairos-index-checked`.
- If a download or a check fails, the builder stays off and the server logs
  one line, `the code index builder did not start`, with the file or the URL
  and the expected checksum. The server runs as usual. The next start tries
  again. The server does not replace a file with an incorrect checksum.
  Remove the file to download it again.

| File in `tools/` | Download from | sha256 |
|---|---|---|
| `Qwen_Qwen3-4B-Instruct-2507-Q4_K_M.gguf` | `https://huggingface.co/bartowski/Qwen_Qwen3-4B-Instruct-2507-GGUF/resolve/ae44f08e1392f39c0e474af10c3ff8355c8b6688/Qwen_Qwen3-4B-Instruct-2507-Q4_K_M.gguf` | `2fde00ce69dd4899c70d020845e2638353015bba0fdf161b3eb965f2bca4464e` |
| `rust-analyzer-<target>.gz` | `https://github.com/rust-lang/rust-analyzer/releases/download/2026-09-28/rust-analyzer-<target>.gz` | in `crates/kairos-index/src/rust_analyzer.rs` |
| `rust-src-1.99.0.tar.gz` | `https://static.rust-lang.org/dist/2026-10-01/rust-src-1.99.0.tar.gz` | `82b978093b33c71bcbe69005b92181bfb76d3d0e434ef303e637812e21badcd7` |
| `rustc-1.93.0-<target>.tar.gz`, `cargo-1.93.0-<target>.tar.gz`, `rust-std-1.93.0-<target>.tar.gz` | `https://static.rust-lang.org/dist/<file>` | in `crates/kairos-index/src/tools.rs` |

`<target>` is `aarch64-unknown-linux-gnu` or `x86_64-unknown-linux-gnu` for
the image. The server unpacks rust-analyzer to `tools/rust-analyzer` and the
3 Rust archives to `tools/rust-1.93.0/`.

**With no network:** download the files on a computer with network access.
Keep their names. Put them in `KAIROS_CODE_INDEX_DIR/tools/` before the
start. The user of the server (uid 10001 in the image) must be able to write
the folder. Then the server downloads nothing.

### Development

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_DEV_UI` | `true` \| `1` \| `false` \| `0` | `false` | Mounts the Swagger UI at `/api/docs`. At the default the route does not exist. Any other value fails startup. |
| `KAIROS_WEB_DIST` | directory path | unset | Serve the GUI from this directory instead of the assets embedded in the binary. When both are present, the directory wins. |

### Retention

The retention pass runs each 24 hours. The interval is fixed. The pass is a
periodic job (see [Job queue](#job-queue)): one server runs each pass. The
first start of a deployment runs the pass at once. One pass:

- In each tenant, removes the `item_history` rows and the `activity_log` rows
  that the five variables below make prune candidates. The mode sets what
  happens to them first.
- Deletes the password sessions (`local_sessions`) that expired before the
  pass. This occurs in each mode, also `off`, because an expired session
  cannot log in.
- Writes one `retention_sweep` row to the `activity_log` of each tenant that
  it sweeps, with the counts of the rows it archived and removed.
- Logs the counts for each tenant and the number of deleted sessions.

When there are 2 or more replicas, only one replica runs a pass. A failed
pass is logged and tried again. The next pass runs on time. A value that the
server cannot parse stops the start.

Their parsing differs from the sixteen variables above: empty is not treated as
unset. An empty `KAIROS_HISTORY_HOT_DAYS`, `KAIROS_HISTORY_KEEP_LATEST`,
`KAIROS_ACTIVITY_RETENTION_DAYS` or `KAIROS_RETENTION_MODE` is a parse error
rather than a fallback to the default. `KAIROS_ARCHIVE_TARGET` is the one
exception: empty means no target.

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_HISTORY_HOT_DAYS` | unsigned integer (days) | `90` | Hot window. `item_history` snapshots edited within this window are never touched. |
| `KAIROS_HISTORY_KEEP_LATEST` | unsigned integer | `5` | Latest-N guard. The newest N versions of an item are never prune candidates regardless of age. |
| `KAIROS_ACTIVITY_RETENTION_DAYS` | unsigned integer (days) | `365` | `activity_log` window. Rows older than this are archive-then-delete candidates, with no compaction tiers. |
| `KAIROS_ARCHIVE_TARGET` | filesystem path or `s3://…` URL | unset | Where prune candidates are offloaded as NDJSON. A `file://` prefix is stripped. An `s3://` value parses to a distinct S3 target that the shipped code rejects with a typed not-implemented error, rather than being misread as a directory name. Empty means no target. |
| `KAIROS_RETENTION_MODE` | `archive` \| `discard` \| `off` | `archive` | `archive` offloads before pruning and, with no target configured, prunes nothing and logs a warning instead. `discard` prunes without archiving. `off` disables the sweeper entirely. Case-insensitive; any other value fails to parse. |

Past the hot window, compaction keeps the first and last snapshot per item per
UTC calendar month and treats the intermediate versions as prune candidates.
See [Archiving](../explanation/archiving.md).

### Job queue

Each server runs a worker for the job queue (KAIROS-T-0373). The jobs are in
the table `public.jobs`. All replicas share the table. A worker takes the next
job with the highest priority. In one priority, the oldest job goes first. Two
workers never take the same job.

A job has a lease. The job tells the worker when it makes progress. At each
heartbeat, the worker writes the progress and extends the lease. When a job
makes no progress for the lease time, its worker stops it. Then the job goes
back to the queue.

When a server stops, the lease of its jobs runs out. Then another worker takes
those jobs. After the last try, the job fails, and the
row keeps the reason.

The embedding refresh uses the queue too. At each `KAIROS_EMBED_REFRESH_SECS`,
each server adds the next embedding batch of each organization. An
organization has one open batch at a time, and the next batch starts one
interval after the last batch ended. Thus 2 servers do not embed an item two
times. When a batch fails after its tries, its items stay stale, and the next
batch starts at the same item.

A periodic pass (retention, and the sweep of the code index) is a job that
adds its next run when it ends. A pass has one open run at a time. The next
run is due one interval after the last run was due. When a server starts, it
adds a run of each pass that has no open run. When a server stops during a
pass, another server runs the pass again after the lease. A server that
cannot run a pass (for example, its code-index tools are not ready) gives
the pass back to the queue, and no try is counted.

A worker removes a job 7 days after it ends. A value that is not a number, or
that is out of range, stops the start of the server.

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_JOB_HEARTBEAT_SECS` | whole number, 1 or more | `30` | How often the worker writes the progress of a job. |
| `KAIROS_JOB_LEASE_SECS` | whole number, 2 heartbeats or more | `300` | The time that a job can run with no progress. The worker stops the job one heartbeat before this time. |
| `KAIROS_JOB_MAX_TRIES` | whole number, 1 or more | `3` | The number of tries of a job. After the last try, the job fails. |
| `KAIROS_JOB_LIMIT_EMBEDDED_BUILD` | whole number | `1` | The code-index builds with the embedded model that one server runs at the same time. `0`: the server does not run them. |
| `KAIROS_JOB_LIMIT_HOSTED_BUILD` | whole number | `4` | The code-index builds with a hosted provider that one server runs at the same time. |
| `KAIROS_JOB_LIMIT_EMBEDDING_BATCH` | whole number | `2` | The embedding batches that one server runs at the same time. |
| `KAIROS_JOB_LIMIT_PERIODIC` | whole number | `1` | The periodic passes that one server runs at the same time. |

## Default board configurations

Provisioning a tenant seeds one configuration per flight level, and creating a
board copies its level's. These are the defaults; a board's columns and
transitions are data and may be changed afterwards.

| Level | Columns | Transitions |
|---|---|---|
| `strategy` | Draft, Review, Active, Monitoring, Completed | Draft → Review → Active → Monitoring → Completed |
| `initiative` | Discovery, Design, Ready, Decompose, Active, Monitoring, Completed | Discovery → Design → Ready → Decompose → Active → Monitoring → Completed |
| `delivery` | Backlog, Todo, Blocked, Active, Completed | Backlog → Todo → Active → Completed, plus Todo ↔ Blocked and Active ↔ Blocked |
| `adr` | Draft, Discussion, Decided, Superseded | Draft → Discussion → Decided → Superseded |

The delivery graph is the only one that is not a straight line: Blocked is
reachable from Todo and from Active, and returns to whichever it came from.
The strategy, initiative and ADR graphs are forward-only, so an item cannot be
moved back a column — a correction is a new item, not a reversal.

Stored in `public.system_board_defaults`, one row per level.

## CLI configuration

The CLI reads no server variables. Its configuration surface is where it keeps
cached credentials, and the settings of `kairos index update` below.

| Variable | Description |
|---|---|
| `KAIROS_CONFIG_DIR` | The config directory, taken verbatim. Highest precedence. |
| `XDG_CONFIG_HOME` | Used as `$XDG_CONFIG_HOME/kairos` when `KAIROS_CONFIG_DIR` is unset or empty. |
| `HOME` | Used as `$HOME/.config/kairos` when neither of the above is set. |

With none of the three set, any command that reaches the API fails with exit 1
and a message naming all three.

The directory is created mode `0700`, and `credentials.json` inside it is
written mode `0600` — both at creation and on every rewrite, including the
token-refresh path. Writes are atomic: a temporary file is written and then
renamed.

### `credentials.json`

```json
{
  "version": 1,
  "deployments": {
    "https://kairos.example.com": {
      "access_token": "eyJ…",
      "refresh_token": "…",
      "expires_at": 1793491200,
      "issuer": "https://idp.example.com",
      "client_id": "kairos-cli",
      "tenant": "acme",
      "api_bearer": "access_token"
    }
  }
}
```

| Field | Type | Description |
|---|---|---|
| `version` | integer | Store format version. `1`. Defaults to `1` when absent. |
| `deployments` | object | One entry per deployment, keyed by the normalized deployment URL — whitespace and trailing slashes trimmed. Serialized in sorted key order. |
| `deployments.*.access_token` | string | The token sent as the API bearer. Holds whichever token `api_bearer` names. |
| `deployments.*.refresh_token` | string, optional | Omitted when the issuer declined `offline_access`. |
| `deployments.*.expires_at` | integer | Access-token expiry, in Unix seconds. |
| `deployments.*.issuer` | string | The OIDC issuer the tokens came from. Refreshes go back to it. |
| `deployments.*.client_id` | string | The OAuth client id used for the device grant and for refresh. |
| `deployments.*.tenant` | string, optional | Sent as `X-Tenant`. Omitted when no tenant was cached at login. |
| `deployments.*.api_bearer` | `access_token` \| `id_token` | Which token `access_token` holds. Absent in files written before the field existed, which resolve to `access_token`. |
| `deployments.*.kind` | `oauth` \| `local_session` | How the entry was made. Omitted for `oauth`. `local_session` is an entry from `kairos login --email`. |
| `deployments.*.email` | string, optional | The email of a local session. Omitted for `oauth`. |

An entry for a local session holds the session bearer in `access_token`.
`expires_at` holds the expiry that the server gave. The entry has no
`refresh_token`. `issuer` and `client_id` are empty strings.

```json
{
  "access_token": "kairos_ss_…",
  "expires_at": 1793491200,
  "issuer": "",
  "client_id": "",
  "tenant": "acme",
  "api_bearer": "access_token",
  "kind": "local_session",
  "email": "you@example.com"
}
```

A token is refreshed when its `expires_at` is within 30 seconds of now, so a
token never expires mid-request. The CLI does not refresh a local session. The
CLI uses it until its `expires_at`. After that, each command fails with exit 2
and gives the `kairos login` command. A file that does not parse as JSON is an
authentication error (exit 2) naming the file.

### `kairos index update` and the base index

`kairos index update` asks Kairos for the nearest base index of the checkout.
It downloads that index if the checkout has no index. It also downloads it if
the base is nearer to the tree than the local index. Then it indexes the
changed files.

When the local index is as near to the tree as the base, the CLI keeps the
local index. The base can have more summaries than the local index links.
Then the CLI downloads the base and takes its summaries into the local
index. The output says so. The tool `fetch_index` of the code tools does the same.

| Setting | Description |
|---|---|
| `--url`, then `KAIROS_URL`, then `deployment_url` in `.claude/kairos.local.md` | The Kairos deployment. With none of them, the CLI uses the deployment of `kairos login`. |
| `KAIROS_KEY`, then `KAIROS_MCP_KEY` | An agent key or a service-account key, sent as the bearer, as for each other command ([CLI → Authentication](cli.md#authentication)). With neither, the CLI uses the credentials of `kairos login`. |
| `--repository`, then `repository` in `.claude/kairos.local.md` | The repository in Kairos. With neither, the CLI uses the repository whose `repo_url` is the `origin` remote of the checkout. |
| `--max-changed` | The most files that can change since the base index. The default is 200. |
| `--link-only` | Do not run the summary model. The update links the summaries that are in the pool of the index, and it does not make new summaries. The background update of the Claude Code plugin uses this option. |

A repository can opt in to the hosted provider of its organization
(`code_index_summaries = hosted`). On such a repository, the update makes
no summary, whatever `KAIROS_INDEX_SUMMARIZE` says. It links the summaries
of the pool, and its last line says so. The structure of the checkout is
still built for the changed files. So search and the call graph are right
on a branch before its push.

The next push makes the summaries on Kairos. The CLI never has the key of
the organization. `kairos index build` and `kairos index --full` do not ask
Kairos. They run the model of the machine when it has one.

The base is the nearest indexed commit at or below the merge base of `HEAD`
and the default branch. If more files than the limit changed since that
commit, the CLI builds nothing. It tells you to rebase on the default branch,
or to run `kairos index --full`. It also gives the time of a full build. If
the CLI cannot reach Kairos, it updates the local index and says so.

## Helm chart values

`deploy/helm/kairos/values.yaml`. Most values under `config` become entries in a
ConfigMap that the Deployment loads with `envFrom`. Two exceptions: the
`config.webClientSecret*` values never reach the ConfigMap — the secret is
Secret-sourced, like `DATABASE_URL` — and the five `config.retention.*` values
plus the two `config.otel.*` values are emitted only when non-empty. The chart
provides no identity provider, and
provides PostgreSQL only as an evaluation convenience you can decline
(`postgresql.enabled`).

### Bundled database

| Value | Type | Default | Description |
|---|---|---|---|
| `postgresql.enabled` | bool, optional | *(unset)* | Tri-state. Unset: on **unless** `database.url`/`existingSecret` is set. `true`: on, and naming an external database is refused at render time. `false`: off. Evaluation only — one replica, no backups. |
| `postgresql.image.repository` / `.tag` | string | `pgvector/pgvector` / `pg16` | Must carry `pgvector`; the plain `postgres` image does not. |
| `postgresql.auth.database` / `.username` / `.password` | string | `kairos` / `kairos` / `kairos-evaluation-only` | The password is in values and in release history by construction. Fixed rather than generated so an upgrade does not change it out from under the volume. |
| `postgresql.persistence.size` | string | `8Gi` | `0` uses an `emptyDir`, which loses the data when the pod restarts. |
| `postgresql.persistence.storageClass` | string | `""` | Cluster default when empty. |

### Semantic retrieval

The local model ships inside the image, so retrieval works with none of these
set. `KAIROS_EMBED_API_KEY` is Secret-sourced like `DATABASE_URL` and never
reaches the ConfigMap.

| Value | Env var | Default | Description |
|---|---|---|---|
| `embeddings.provider` | `KAIROS_EMBED_PROVIDER` | — | `local` \| `remote` \| `none`. Empty means local. `none` disables embeddings; search still works from text alone. |
| `embeddings.url` | `KAIROS_EMBED_URL` | — | OpenAI-compatible base URL. Setting it selects the remote provider on its own. |
| `embeddings.model` | `KAIROS_EMBED_MODEL` | — | Model name to request from that endpoint. |
| `embeddings.apiKey` / `embeddings.existingSecret` (+`existingSecretKey`) | `KAIROS_EMBED_API_KEY` | — | **Secret.** A local Ollama needs none. |

The server reads two more that the chart does not surface, because a deployment
should not normally need them: `KAIROS_EMBED_CACHE` (where the local model lives;
the image sets it to `/var/lib/kairos/models`) and `KAIROS_EMBED_REFRESH_SECS`
(how often the background refresher looks for work, default 10, `0` disables).

### Workload

| Value | Type | Default | Description |
|---|---|---|---|
| `replicaCount` | integer | `1` | Server replicas. Ignored when `autoscaling.enabled` is true. Keep 1 until the job queue (COLLIERY-I-0613) ships: with 2 or more, each pod runs its own code-index builder, embedding refresher and login throttle. |
| `image.repository` | string | `ghcr.io/colliery-io/kairos` | Image repository. |
| `image.tag` | string | `""` | Empty tracks the chart's `appVersion`. An explicit value pins a published release, e.g. `"0.10.0"`. The chart never pins `latest`. |
| `image.pullPolicy` | string | `IfNotPresent` | Image pull policy. |
| `imagePullSecrets` | list | `[]` | Pull secrets, e.g. `[{name: ghcr-creds}]`. |
| `nameOverride` | string | `""` | Overrides the chart name used in resource names. |
| `fullnameOverride` | string | `""` | Overrides the fully-qualified release name used in resource names. |
| `resources.requests.cpu` | quantity | `100m` | CPU request. |
| `resources.requests.memory` | quantity | `128Mi` | Memory request. |
| `resources.limits.cpu` | quantity | `"1"` | CPU limit. |
| `resources.limits.memory` | quantity | `512Mi` | Memory limit. |
| `podAnnotations` | map | `{}` | Extra pod annotations. |
| `podLabels` | map | `{}` | Extra pod labels. |
| `nodeSelector` | map | `{}` | Node selector for pod scheduling. |
| `tolerations` | list | `[]` | Tolerations for pod scheduling. |
| `affinity` | map | `{}` | Affinity rules for pod scheduling. |
| `topologySpreadConstraints` | list | `[]` | Topology spread constraints, e.g. spreading replicas across zones. |
| `extraEnv` | list of EnvVar | `[]` | Appended verbatim to the container. The route for any variable the chart does not surface. |
| `tmp.enabled` | bool | `true` | Mounts an emptyDir at `/tmp`. The root filesystem is read-only, and the code index builder, index uploads and the git helper make temporary files. Turn it off only when `extraVolumes` and `extraVolumeMounts` give `/tmp`. |
| `tmp.sizeLimit` | quantity | `8Gi` | The size limit of that emptyDir. Empty means no limit. |
| `extraVolumes` | list | `[]` | Extra pod volumes. |
| `extraVolumeMounts` | list | `[]` | Extra container volume mounts. |

### Security context

| Value | Type | Default | Description |
|---|---|---|---|
| `serviceAccount.create` | bool | `true` | Create a dedicated ServiceAccount for the pods. |
| `serviceAccount.name` | string | `""` | Empty uses the generated fullname when `create` is true, and `default` when it is false. |
| `serviceAccount.annotations` | map | `{}` | ServiceAccount annotations, e.g. IRSA or Workload Identity roles. |
| `serviceAccount.automountServiceAccountToken` | bool | `false` | Auto-mount the ServiceAccount token. Kairos does not call the Kubernetes API. |
| `podSecurityContext.runAsNonRoot` | bool | `true` | Pod-level security context. |
| `podSecurityContext.runAsUser` | integer | `65532` | Pod-level security context. |
| `podSecurityContext.runAsGroup` | integer | `65532` | Pod-level security context. |
| `podSecurityContext.fsGroup` | integer | `65532` | Pod-level security context. |
| `podSecurityContext.seccompProfile.type` | string | `RuntimeDefault` | Pod-level seccomp profile. |
| `containerSecurityContext.allowPrivilegeEscalation` | bool | `false` | Container-level security context. |
| `containerSecurityContext.readOnlyRootFilesystem` | bool | `true` | Container-level security context. Temporary files go to the `tmp` volume. |
| `containerSecurityContext.runAsNonRoot` | bool | `true` | Container-level security context. |
| `containerSecurityContext.capabilities.drop` | list | `[ALL]` | Linux capabilities dropped from the container. |

The read-only root filesystem is compatible with a filesystem
`KAIROS_ARCHIVE_TARGET` provided the path is a writable volume mounted through
`extraVolumes` and `extraVolumeMounts`.

### Networking

| Value | Type | Default | Description |
|---|---|---|---|
| `containerPort` | integer | `8080` | The container's listen port. The chart sets `KAIROS_BIND_ADDR` to `0.0.0.0:<containerPort>`. Also the Service `targetPort`. |
| `service.type` | string | `ClusterIP` | Service type. ClusterIP plus an Ingress is the intended topology. |
| `service.port` | integer | `80` | The port the Service exposes in-cluster. |
| `service.annotations` | map | `{}` | Service annotations. |
| `ingress.enabled` | bool | `false` | Create an Ingress. |
| `ingress.className` | string | `""` | IngressClass, e.g. `nginx` or `traefik`. Empty uses the cluster default. |
| `ingress.annotations` | map | `{}` | Ingress annotations. For cert-manager TLS, e.g. `cert-manager.io/cluster-issuer`. |
| `ingress.tenancyMode` | `wildcard` \| `single` | `wildcard` | `wildcard` routes `*.<config.tenancy.baseDomain>` and the apex, which needs wildcard DNS and a wildcard TLS certificate. `single` routes `ingress.host` only. |
| `ingress.host` | hostname | `kairos.example.com` | Used in `single` mode. In `wildcard` mode the host rules derive from `config.tenancy.baseDomain`. |
| `ingress.path` | string | `/` | HTTP path prefix. Kairos serves everything from `/`. |
| `ingress.pathType` | string | `Prefix` | Ingress path type. |
| `ingress.tls.enabled` | bool | `false` | Emit a TLS block on the Ingress. |
| `ingress.tls.secretName` | string | `""` | Name of the TLS Secret. Empty lets the ingress controller's default certificate apply. |

### Scaling and metrics

| Value | Type | Default | Description |
|---|---|---|---|
| `autoscaling.enabled` | bool | `false` | When true, `replicaCount` is ignored. Keep it off until the job queue (COLLIERY-I-0613) ships: the HPA runs 2 or more pods. |
| `autoscaling.minReplicas` | integer | `2` | HorizontalPodAutoscaler floor. |
| `autoscaling.maxReplicas` | integer | `6` | HorizontalPodAutoscaler ceiling. |
| `autoscaling.targetCPUUtilizationPercentage` | integer | `70` | Percentage of the CPU request. |
| `autoscaling.targetMemoryUtilizationPercentage` | integer or null | `null` | `null` disables the metric. |
| `metrics.serviceMonitor.enabled` | bool | `false` | Creates a Prometheus Operator ServiceMonitor for `/metrics`. Requires the `monitoring.coreos.com` CRDs. |
| `metrics.serviceMonitor.interval` | duration | `30s` | Scrape interval. |
| `metrics.serviceMonitor.scrapeTimeout` | duration | `10s` | Scrape timeout. |
| `metrics.serviceMonitor.labels` | map | `{}` | Extra labels, e.g. to match a Prometheus `serviceMonitorSelector`. |

### Probes

| Value | Type | Default | Description |
|---|---|---|---|
| `livenessProbe` | Probe | `httpGet` `/healthz` on port `http`, `initialDelaySeconds: 5`, `periodSeconds: 10`, `timeoutSeconds: 3`, `failureThreshold: 3` | Passed through verbatim. `/healthz` is process-only. |
| `readinessProbe` | Probe | `httpGet` `/readyz` on port `http`, `initialDelaySeconds: 5`, `periodSeconds: 10`, `timeoutSeconds: 3`, `failureThreshold: 3` | Passed through verbatim. `/readyz` checks the database and pending migrations. |
| `startupProbe.enabled` | bool | `true` | Emit a startupProbe, which covers slow first-boot migrations before liveness starts counting. |
| `startupProbe` | Probe | `httpGet` `/readyz` on port `http`, `periodSeconds: 5`, `failureThreshold: 30` | Passed through verbatim minus `enabled`. Allows up to `failureThreshold × periodSeconds` for the first successful `/readyz`. |

### Application configuration

| Value | Type | Default | Description |
|---|---|---|---|
| `config.oidc.issuerUrl` | string | `""` | Sets `OIDC_ISSUER_URL`. Required **unless** the chart is bundling a Dex (`dex.enabled`) or `config.localAuth.enabled` is on; rendering fails when none of the three gives anyone a way to log in. |
| `config.oidc.audience` | string or list | `""` | Sets `OIDC_AUDIENCE`. Required. A list is joined with commas. |
| `config.webClientId` | string | `kairos-web` | Sets `KAIROS_WEB_CLIENT_ID`. |
| `config.apiBearer` | string | `access_token` | Sets `KAIROS_API_BEARER`. |
| `config.webClientSecret` | string | `""` | Sets `KAIROS_WEB_CLIENT_SECRET` through a chart-rendered Secret. Never enters the ConfigMap. |
| `config.webClientSecretExistingSecret` | string | `""` | Name of a pre-existing Secret holding the value. When set, the chart renders none of its own. |
| `config.webClientSecretExistingSecretKey` | string | `KAIROS_WEB_CLIENT_SECRET` | Key within that Secret. |
| `config.tenancy.baseDomain` | string | `""` | Sets `KAIROS_BASE_DOMAIN`. Emitted only when non-empty. |
| `config.tenancy.singleTenant` | string | `""` | Sets `KAIROS_SINGLE_TENANT`. Emitted only when non-empty. |
| `config.tenancy.singleTenantName` | string | `""` | Sets `KAIROS_SINGLE_TENANT_NAME`. Emitted only when non-empty. |
| `config.tenancy.autoJoinDomains` | list or string | `[]` | Sets `KAIROS_AUTO_JOIN_DOMAINS`; a list is joined with commas. Emitted only when non-empty. Rendering fails when it is set without `singleTenant`. |
| `config.deploymentAdmins` | string | `""` | Sets `KAIROS_DEPLOYMENT_ADMINS`. |
| `config.log.level` | string | `info` | Sets `KAIROS_LOG_LEVEL`. |
| `config.log.format` | string | `json` | Sets `KAIROS_LOG_FORMAT`. |
| `config.devUi` | bool | `false` | Sets `KAIROS_DEV_UI`. |
| `config.otel.endpoint` | string | `""` | Sets `KAIROS_OTEL_ENDPOINT`. Emitted only when non-empty; empty disables tracing. |
| `config.otel.sampleRatio` | string | `""` | Sets `KAIROS_OTEL_SAMPLE_RATIO`. Emitted only when non-empty. |
| `config.auth.maxFailures` | integer or `""` | `""` | Sets `KAIROS_AUTH_MAX_FAILURES`. Emitted when set, **including `0`**, which turns throttling off. |
| `config.auth.failureWindowSecs` | integer or `""` | `""` | Sets `KAIROS_AUTH_FAILURE_WINDOW_SECS`. Emitted only when set. |
| `config.auth.lockoutSecs` | integer or `""` | `""` | Sets `KAIROS_AUTH_LOCKOUT_SECS`. Emitted only when set. |
| `config.localAuth.enabled` | bool | `false` | Sets `KAIROS_LOCAL_AUTH`. |
| `config.localAuth.sessionTtlSecs` | integer or `""` | `""` | Sets `KAIROS_SESSION_TTL_SECS`. Emitted only when set. |
| `config.localAuth.bootstrapAdmin` | string | `""` | Sets `KAIROS_BOOTSTRAP_ADMIN`. Rendering fails if it is set without a password, or a password without it. |
| `config.localAuth.bootstrapPasswordHash` | string | `""` | The first-boot password as a PHC hash, rendered into a Secret (never the ConfigMap). Set it in a values file, not with `--set` — a PHC string contains commas. |
| `config.localAuth.bootstrapPasswordExistingSecret` | string | `""` | Name of a Secret holding it instead. Wins over the above. |
| `config.localAuth.bootstrapPasswordExistingSecretKey` | string | `KAIROS_BOOTSTRAP_PASSWORD_HASH` | Key within that Secret. |
| `config.auth.trustedProxy` | bool or `""` | `""` | Sets `KAIROS_TRUSTED_PROXY`. Empty **follows `ingress.enabled`**: behind an Ingress the socket peer is the controller, so the header is the only real client address; with no Ingress the header is caller-supplied. Set it explicitly for a gateway of your own. |
| `config.retention.historyHotDays` | integer or null | `null` | Sets `KAIROS_HISTORY_HOT_DAYS`. Emitted only when non-empty. |
| `config.retention.historyKeepLatest` | integer or null | `null` | Sets `KAIROS_HISTORY_KEEP_LATEST`. Emitted only when non-empty. |
| `config.retention.activityRetentionDays` | integer or null | `null` | Sets `KAIROS_ACTIVITY_RETENTION_DAYS`. Emitted only when non-empty. |
| `config.retention.archiveTarget` | string | `""` | Sets `KAIROS_ARCHIVE_TARGET`. Emitted only when non-empty. |
| `config.retention.mode` | string | `""` | Sets `KAIROS_RETENTION_MODE`. Emitted only when non-empty. |
| `database.url` | string | `""` | Sets `DATABASE_URL` through a chart-rendered Secret. |
| `database.existingSecret` | string | `""` | Name of a pre-existing Secret holding it. When set, the chart renders none of its own and this value wins over `database.url`. |
| `database.existingSecretKey` | string | `DATABASE_URL` | Key within that Secret. |

The chart's two rendering guards:

- **Tenancy.** Exactly one of `config.tenancy.baseDomain` and
  `config.tenancy.singleTenant`. Rendering fails when both are set and when
  neither is.
- **Database.** At least one of `database.url` and `database.existingSecret`.
  Rendering fails only when neither is set; with both set,
  `database.existingSecret` is used and no Secret is rendered.

Retention values left `null` or `""` are absent from the ConfigMap. Then the
server uses its defaults.

## Reference Compose deployment

`deploy/docker-compose.yaml` with `deploy/.env`, copied from
`deploy/.env.example`. The topology is Caddy for TLS and subdomain routing,
one Kairos container, and PostgreSQL 16 as the sole state — as
`pgvector/pgvector:pg16`, because Kairos requires the `pgvector` extension and
stock `postgres:16` does not carry it. There is no bundled identity provider.

| `.env` variable | Default in the example | Consumed by |
|---|---|---|
| `KAIROS_VERSION` | `0.10.0` | The Kairos image tag. Required — Compose fails if it is unset. |
| `KAIROS_SITE_ADDRESS` | `kairos.example.com` | Caddy's site address. A hostname enables automatic HTTPS; `:80` is a local no-TLS trial. Required. |
| `POSTGRES_PASSWORD` | `change-me` | The Postgres password, and the password inside the `DATABASE_URL` the Compose file composes. Required. |
| `OIDC_ISSUER_URL` | `https://idp.example.com/` | The server, verbatim. Required. |
| `OIDC_AUDIENCE` | `kairos` | The server, verbatim. Required. |
| `KAIROS_WEB_CLIENT_ID` | `kairos-web` | The server. Falls back to `kairos-web`. |
| `KAIROS_BASE_DOMAIN` | `kairos.example` | The server. Defaults to empty. |
| `KAIROS_SINGLE_TENANT` | commented out | The server. Defaults to empty. |
| `KAIROS_SINGLE_TENANT_NAME` | commented out | The server. Defaults to empty. |
| `KAIROS_AUTO_JOIN_DOMAINS` | commented out | The server. Defaults to empty. |
| `KAIROS_DEPLOYMENT_ADMINS` | empty | The server. Defaults to empty. |
| `KAIROS_LOG_LEVEL` | `info` | The server. Falls back to `info`. |
| `KAIROS_API_BEARER` | `access_token` | Nothing. Present in `.env.example` but **not forwarded** to the container by `deploy/docker-compose.yaml`. |
| `KAIROS_WEB_CLIENT_SECRET` | empty | Nothing. Present in `.env.example` but **not forwarded** to the container by `deploy/docker-compose.yaml`. |

The Compose file also fixes two values that have no `.env` entry:
`KAIROS_BIND_ADDR` is `0.0.0.0:8080` and `KAIROS_LOG_FORMAT` is `json`. The
image applies pending migrations on boot, before binding, so no separate
migration step exists in this deployment.

## Related reading

- [CLI](cli.md) — the commands that read the CLI configuration above
- [Archiving](../explanation/archiving.md)
- [Capabilities and access](../explanation/capabilities-and-access.md)
- [Glossary](glossary.md)

## Related guides

- [Install with Helm](../how-to/install-with-helm.md)
- [Configure an OIDC issuer](../how-to/configure-an-oidc-issuer.md)
- [Provision a tenant](../how-to/provision-a-tenant.md)
- [Connect a git forge](../how-to/connect-a-git-forge.md)
- [Back up and restore](../how-to/back-up-and-restore.md)
