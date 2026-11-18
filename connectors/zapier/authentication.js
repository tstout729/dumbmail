'use strict';

// DumbMail API keys (Settings → Developers) go in an Authorization: Bearer header.
// Self-hosted DumbMail servers set their own address.
const baseUrl = (bundle) => (bundle.authData.base_url || 'https://dumbmail.ai').replace(/\/+$/, '');

const addAuthHeader = (request, z, bundle) => {
  if (bundle.authData.api_key) {
    request.headers.Authorization = `Bearer ${bundle.authData.api_key}`;
  }
  request.headers.Accept = 'application/json';
  return request;
};

// Turn DumbMail's error body ({error: {code, message}}) into a readable Zapier error.
const handleErrors = (response, z) => {
  // Callers that check the status themselves ask for that with skipThrowForStatus.
  if (response.skipThrowForStatus || (response.request && response.request.skipThrowForStatus)) return response;
  if (response.status === 401) {
    throw new z.errors.Error('The API key was refused. Check it in DumbMail under Settings → Developers.', 'AuthenticationError', response.status);
  }
  if (response.status >= 400) {
    let message = `DumbMail returned HTTP ${response.status}`;
    try {
      const body = typeof response.json === 'object' && response.json ? response.json : JSON.parse(response.content);
      if (body && body.error && body.error.message) message = body.error.message;
    } catch (e) {
      // Not JSON; keep the status message.
    }
    throw new z.errors.Error(message, response.status >= 500 ? 'ServerError' : 'RequestError', response.status);
  }
  return response;
};

const test = async (z, bundle) => {
  if (!/^https:\/\/[^/\s]+/i.test(baseUrl(bundle))) {
    throw new z.errors.Error('The DumbMail address must start with https://', 'InvalidAddress', 400);
  }
  const response = await z.request({ url: `${baseUrl(bundle)}/api/hooks/me` });
  return response.data;
};

module.exports = {
  baseUrl,
  addAuthHeader,
  handleErrors,
  config: {
    type: 'custom',
    fields: [
      {
        key: 'api_key',
        label: 'API Key',
        required: true,
        type: 'password',
        helpText: 'A private API key from [DumbMail → Settings → Developers](https://app.dumbmail.ai/settings/developers) → Create key. It starts with `pl_sk_`.',
      },
      {
        key: 'base_url',
        label: 'DumbMail Address',
        required: false,
        type: 'string',
        default: 'https://dumbmail.ai',
        helpText: 'Leave as is unless you run your own DumbMail server; then enter its address, e.g. `https://mail.example.com`.',
      },
    ],
    test,
    connectionLabel: (z, bundle) => (bundle.inputData.account && bundle.inputData.account.name) || 'DumbMail',
  },
};
