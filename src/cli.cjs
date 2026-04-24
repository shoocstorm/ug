#!/usr/bin/env node

const { join, dirname } = require('path');

const binding = join(dirname(__dirname), 'native', 'ultragraph-kb.node');

const native = require(binding);

const pathArg = process.argv[2] || '.';
const result = native.index(pathArg);
console.log(result);