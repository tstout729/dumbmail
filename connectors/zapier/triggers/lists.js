'use strict';

const { baseUrl } = require('../authentication');

// Hidden trigger that fills the List dropdown in actions.
module.exports = {
  key: 'list',
  noun: 'List',
  display: { label: 'New List', description: 'Lists in your DumbMail account.', hidden: true },
  operation: {
    perform: async (z, bundle) => {
      const response = await z.request({ url: `${baseUrl(bundle)}/api/lists` });
      return response.data.data.map((l) => ({ id: l.id, name: l.name, member_count: l.member_count }));
    },
    sample: { id: '0193a5b4-9d40-7b88-9f44-5c6d7e8f9a01', name: 'Newsletter', member_count: 1200 },
    outputFields: [{ key: 'id', label: 'List ID' }, { key: 'name', label: 'Name' }],
  },
};
