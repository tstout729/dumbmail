'use strict';

const { baseUrl } = require('../authentication');

module.exports = {
  key: 'event',
  noun: 'Event',
  display: { label: 'Track Event', description: 'Records an event (like Placed Order) for a person. Events can start flows and build segments.' },
  operation: {
    inputFields: [
      { key: 'metric', label: 'Metric', required: true, helpText: 'The event name, e.g. `Placed Order` or `Booked Demo`.' },
      { key: 'email', label: 'Email', required: false },
      { key: 'phone_number', label: 'Phone Number', required: false },
      { key: 'external_id', label: 'External ID', required: false },
      { key: 'value', label: 'Value', type: 'number', required: false, helpText: 'Revenue for orders, in your currency.' },
      { key: 'properties', label: 'Properties', dict: true, required: false },
      { key: 'unique_id', label: 'Unique ID', required: false, helpText: 'Makes retries safe: an event with the same metric and unique ID is only recorded once.' },
      { key: 'time', label: 'Time', type: 'datetime', required: false, helpText: 'When it happened. Defaults to now.' },
    ],
    perform: async (z, bundle) => {
      const d = bundle.inputData;
      const profile = Object.fromEntries(Object.entries({ email: d.email, phone_number: d.phone_number, external_id: d.external_id }).filter(([, v]) => v));
      if (Object.keys(profile).length === 0) {
        throw new z.errors.Error('Give an email, phone number or external ID for the person.', 'InvalidData', 400);
      }
      const body = { metric: d.metric, profile, properties: d.properties || {} };
      if (d.value !== undefined && d.value !== null && d.value !== '') body.value = Number(d.value);
      if (d.unique_id) body.unique_id = d.unique_id;
      if (d.time) body.time = d.time;
      const response = await z.request({ url: `${baseUrl(bundle)}/api/events`, method: 'POST', body });
      return response.data;
    },
    sample: { id: '0193a5b4-8c30-7a77-8e33-4b5c6d7e8f90', profile_id: '0193a5b4-6f1e-7c3a-9d2e-1b2c3d4e5f60' },
  },
};
