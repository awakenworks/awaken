// Records exact arguments; deliberately has no shell or requested-file executor.
import fs from 'node:fs/promises';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { z } from 'zod';
const server = new McpServer({name:'awaken_session',version:'1'});
for (const [name, inputSchema] of [
  ['bash', {command:z.string()}],
  ['write', {file_path:z.string(), content:z.string()}],
]) {
  server.registerTool(name,{description:'Record fixture arguments',inputSchema},async (input) => {
    await fs.appendFile(process.argv[2],JSON.stringify(input)+'\n');
    return {content:[{type:'text',text:'AWAKEN_ALIAS_OK'}]};
  });
}
await server.connect(new StdioServerTransport());
