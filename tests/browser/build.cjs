const { execFileSync } = require('node:child_process');
const { resolve } = require('node:path');

module.exports = () => {
  if (!process.env.BUILDER_TEST_BINARY) {
    execFileSync('cargo', ['build', '--locked', '--bin', 'builder'], {
      cwd: resolve(__dirname, '../..'), stdio: 'inherit',
    });
  }
};
