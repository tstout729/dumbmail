'use strict';

const { baseUrl } = require('../authentication');

module.exports = {
  key: 'profile',
  noun: 'Profile',
  display: { label: 'Find Profile', description: 'Finds a profile by email, phone number, name or external ID.' },
  operation: {
    inputFields: [{ key: 'query', label: 'Email, Phone, Name or External ID', required: true }],
    perform: async (z, bundle) => {
      const response = await z.request({ url: `${baseUrl(bundle)}/api/profiles`, params: { q: bundle.inputData.query, limit: 10 } });
      const q = bundle.inputData.query.trim().toLowerCase();
      const rows = response.data.data;
      // An exact email match first.
      return rows.sort((a, b) => Number((b.email || '').toLowerCase() === q) - Number((a.email || '').toLowerCase() === q));
    },
    sample: { id: '0193a5b4-6f1e-7c3a-9d2e-1b2c3d4e5f60', email: 'jane@example.com', first_name: 'Jane', last_name: 'Doe', email_consent: 'subscribed' },
  },
};
