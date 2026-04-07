let nextA2aMessageId = 0;

export function a2aSendMessagePayload(text: string, taskId?: string) {
  nextA2aMessageId += 1;
  const effectiveTaskId = taskId ?? `e2e-a2a-${Date.now()}-${nextA2aMessageId}`;
  return {
    taskId: effectiveTaskId,
    data: {
      message: {
        taskId: effectiveTaskId,
        contextId: effectiveTaskId,
        messageId: `msg-${effectiveTaskId}`,
        role: 'ROLE_USER',
        parts: [{ text }],
      },
      configuration: {
        returnImmediately: true,
      },
    },
  };
}
