'use strict';

const { baseUrl } = require('../authentication');

const clean = (o) => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined && v !== null && v !== ''));

module.exports = {
  key: 'profile',
  noun: 'Profile',
  display: { label: 'Create or Update Profile', description: 'Adds a profile, or updates the one with the same email, phone number or external ID.' },
  operation: {
    inputFields: [
      { key: 'email', label: 'Email', required: false },
      { key: 'phone_number', label: 'Phone Number', required: false, helpText: 'In international format, e.g. +15035550100.' },
      { key: 'external_id', label: 'External ID', required: false, helpText: 'Your own id for this person, e.g. a customer id.' },
      { key: 'first_name', label: 'First Name', required: false },
      { key: 'last_name', label: 'Last Name', required: false },
      { key: 'properties', label: 'Custom Properties', dict: true, required: false },
      {
        key: 'email_consent',
        label: 'Email Consent',
        required: false,
        choices: { subscribed: 'Subscribed', unsubscribed: 'Unsubscribed' },
        helpText: 'Only set Subscribed if this person agreed to get marketing email from you.',
      },
      { key: 'list_id', label: 'Add to List', required: false, dynamic: 'list.id.name' },
    ],
    perform: async (z, bundle) => {
      const d = bundle.inputData;
      if (!d.email && !d.phone_number && !d.external_id) {
        throw new z.errors.Error('Give an email, phone number or external ID.', 'InvalidData', 400);
      }
      const body = clean({
        email: d.email,
        phone_number: d.phone_number,
        external_id: d.external_id,
        first_name: d.first_name,
        last_name: d.last_name,
        properties: d.properties,
        email_consent: d.email_consent,
        list_ids: d.list_id ? [d.list_id] : undefined,
      });
      const response = await z.request({ url: `${baseUrl(bundle)}/api/profiles`, method: 'POST', body });
      return response.data;
    },
    sample: {
      id: '0193a5b4-6f1e-7c3a-9d2e-1b2c3d4e5f60',
      email: 'jane@example.com',
      phone_number: null,
      external_id: 'cust_1042',
      first_name: 'Jane',
      last_name: 'Doe',
      properties: { city: 'Portland' },
      email_consent: 'subscribed',
      sms_consent: 'never_subscribed',
      created_at: '2026-10-02T16:20:00Z',
      updated_at: '2026-11-17T18:04:05Z',
      last_event_at: null,
    },
  },
};
