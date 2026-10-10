# Rotate keys

This guide tells how to change each key of a Kairos deployment, with no
downtime.

| Key | Who changes it | How |
|---|---|---|
| An agent key | The person of the key | The Agent keys page, or `kairos keys rotate` |
| An API key of a service account | An organization admin | `kairos keys rotate --service-account` |
| `KAIROS_SECRETS_KEY` | The operator | `KAIROS_SECRETS_KEY_OLD` and `kairos-server rotate-secrets` |
| A webhook secret, or `KAIROS_WEBHOOK_SIGNING_KEY` | An organization admin, the operator | `POST /api/forge-connections/{id}/rotate` |

## An agent key or an API key

A rotation makes a new key with the same name and a new secret. The old secret
works until the end of the grace time (1 day, the setting
`KAIROS_API_KEY_ROTATION_GRACE_SECS`). Then it stops.

1. Rotate the key:

   - On the **Agent keys** page, click **Rotate** on the key.
   - Or, for an agent key: `kairos keys rotate <KEY_ID>`.
   - Or, for a key of a service account (an organization admin):
     `kairos keys rotate <KEY_ID> --service-account <SERVICE_ACCOUNT>`.
   - Or the API: `POST /api/me/agent-keys/{key_id}/rotate`, or
     `POST /api/service-accounts/{id}/keys/{key_id}/rotate`. No body.

2. Copy the new secret. Kairos shows it one time only.
3. Put the new secret in the settings of the agent, before the end of the grace
   time. For Claude Code, that is the variable `KAIROS_MCP_KEY`.

The new key has the life of the old key, from now. A key with no expiry gives a
key with no expiry. The list of keys shows the old key as "rotated", with the
id of the new key.

To stop a key now, revoke it. You cannot rotate a revoked key.

### The expiry of a new key

A new key expires after 90 days (the setting `KAIROS_API_KEY_TTL_DAYS`). To set
a different date, send `expires_at` (the CLI option `--expires-at`). To make a
key with no expiry, send `no_expiry: true`. In the CLI, that is the flag
`--no-expiry`. On the Agent keys page, it is the switch **No expiry**.

## `KAIROS_SECRETS_KEY`

`KAIROS_SECRETS_KEY` encrypts the secrets that Kairos keeps in the database. They
are the read token of each repository and the secrets of the hosted models of the
code index. It does not encrypt the API keys: Kairos keeps only their hash.

1. Make a new key:

   ```sh
   openssl rand -base64 32
   ```

2. Set the new key in `KAIROS_SECRETS_KEY`, and the old key in
   `KAIROS_SECRETS_KEY_OLD`. Restart the server. The server encrypts with the
   new key, and decrypts with the two keys.
3. Run the rotation, with the same two settings:

   ```sh
   kairos-server rotate-secrets
   ```

   It encrypts each stored secret of each tenant again with the new key, and
   tells the count for each tenant. `--tenant <slug>` does one tenant only. A
   second run has nothing to do.

4. When the command names no row that it could not open, remove
   `KAIROS_SECRETS_KEY_OLD` and restart the server.

If the command names a row that no key opens, a different key encrypted it, or a
person changed it. Set that secret again: the token of the repository, or the
secret of the code index settings.

Keep `KAIROS_SECRETS_KEY_OLD` until step 4. A server with only the new key cannot
open a row that the old key encrypted.

## A webhook secret, and `KAIROS_WEBHOOK_SIGNING_KEY`

Kairos does not store the secret of a webhook. It makes the secret from
`KAIROS_WEBHOOK_SIGNING_KEY` and the id of the connection. So
`rotate-secrets` has nothing to do for it.

- To change the secret of one webhook, rotate the connection
  (`POST /api/forge-connections/{id}/rotate`). The rotation changes the secret
  and the delivery URL. Put the two in the settings of the webhook on the
  forge. See [Connect a git forge](connect-a-git-forge.md).
- When you change `KAIROS_WEBHOOK_SIGNING_KEY`, each webhook secret changes,
  and each delivery fails until you change the webhook on the forge. Rotate
  each connection, and put the new secret and URL on the forge.
