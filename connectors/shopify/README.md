# DumbMail Shopify app

The Shopify side of DumbMail's Shopify app: its configuration and two extensions. The server side
(OAuth, webhooks, sync) is in `src/features/shopify/`; `docs/SHOPIFY.md` explains all of it.

- `shopify.app.toml`: the app's URLs, scopes and the three privacy webhooks.
- `extensions/dumbmail-embed`: a theme app embed that loads DumbMail's website snippet on every page.
  DumbMail saves the snippet's address in the app's settings in the store (an app-data metafield,
  `dumbmail.script_url`) when the store connects. The merchant turns the embed on once in the theme
  editor; Settings → Shopify links straight to it.
- `extensions/dumbmail-pixel`: a web pixel that reports Started Checkout and Placed Order from checkout
  pages, the same as the custom pixel DumbMail offers to paste (`src/shopify_pixel.js`). DumbMail
  installs it (`webPixelCreate`) with the server's address and the store's public key when the store connects.

Neither is needed for the app to work: without them, the pasted snippet and custom pixel from
Settings → Website do the same job, and webhooks report every order either way.

## Deploying

You need a Shopify Partner account, the app created in the Partner (Dev) Dashboard, Node 20+, and
the [Shopify CLI](https://shopify.dev/docs/api/shopify-cli).

1. In `shopify.app.toml`, set `client_id` to the app's client ID and replace `app.dumbmail.ai` with
   your server's PUBLIC_URL host (in `application_url`, `redirect_urls` and the webhook `uri`).
2. `npm install`, then `shopify app deploy` from this folder. That uploads the configuration and both
   extensions as a new app version and releases it.
3. Set `SHOPIFY_API_KEY` and `SHOPIFY_API_SECRET` on the server (the app's client ID and secret).
4. Stores connected before the extensions were deployed: connect again from Settings → Shopify so
   DumbMail installs the pixel and saves the embed's settings.

When you move to a newer Admin API version, change `api_version` here and `API_VERSION` in
`src/features/shopify/mod.rs` together.

Checking it on a development store: `shopify app dev` serves the extensions to a development store;
the theme editor shows the "DumbMail" app embed and Settings → Customer events shows the pixel.
