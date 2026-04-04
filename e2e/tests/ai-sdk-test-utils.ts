type AiSdkRole = 'system' | 'user' | 'assistant';

let nextMessageId = 0;

export function aiSdkTextPart(text: string) {
  return { type: 'text' as const, text };
}

export function aiSdkFilePart(url: string, mediaType: string, filename?: string) {
  return {
    type: 'file' as const,
    url,
    mediaType,
    ...(filename ? { filename } : {}),
  };
}

export function aiSdkMessage(
  role: AiSdkRole,
  parts: Array<ReturnType<typeof aiSdkTextPart> | ReturnType<typeof aiSdkFilePart>>,
  id?: string,
) {
  nextMessageId += 1;
  return {
    id: id ?? `${role}-${nextMessageId}`,
    role,
    parts,
  };
}

export function aiSdkTextMessage(role: AiSdkRole, text: string, id?: string) {
  return aiSdkMessage(role, [aiSdkTextPart(text)], id);
}

export function aiSdkTextMessages(
  messages: Array<{ role: AiSdkRole; text: string; id?: string }>,
) {
  return messages.map(message => aiSdkTextMessage(message.role, message.text, message.id));
}
