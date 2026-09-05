// Records exact arguments; deliberately has no shell executor.
import fs from 'node:fs/promises';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { z } from 'zod';
const server = new McpServer({name:'awaken_session',version:'1'});
server.registerTool('bash',{description:'Record a fixture command',inputSchema:{command:z.string()}},async (input) => {
  await fs.appendFile(process.argv[2],JSON.stringify(input)+'\n');
  return {content:[{type:'text',text:'AWAKEN_ALIAS_OK'}]};
});
await server.connect(new StdioServerTransport());
