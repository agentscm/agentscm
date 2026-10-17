#!/usr/bin/env node
'use strict';
require('../lib/install.js').main(process.argv.slice(2)).then(
  (code) => process.exit(code),
  (err) => {
    console.error(`agentscm: ${err.message}`);
    process.exit(1);
  },
);
