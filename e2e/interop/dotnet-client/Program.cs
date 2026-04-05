using A2A;
using System.Text.Json;

var baseUrl = args.Length > 0 ? args[0] : "http://127.0.0.1:38080";
using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(90));

A2ACardResolver resolver = new(new Uri(baseUrl));
AgentCard card = await resolver.GetAgentCardAsync(cts.Token);

if (card.SupportedInterfaces.Count == 0)
{
    throw new InvalidOperationException("agent card did not expose a supported interface");
}

IA2AClient client = A2AClientFactory.Create(card);
var taskId = $"dotnet-interop-{DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()}";
SendMessageResponse response = await client.SendMessageAsync(new SendMessageRequest
{
    Message = new Message
    {
        Role = Role.User,
        MessageId = $"msg-{taskId}",
        TaskId = taskId,
        ContextId = taskId,
        Parts = [Part.FromText("hello from dotnet")],
    },
    Configuration = new SendMessageConfiguration
    {
        ReturnImmediately = true,
    },
}, cts.Token);

if (response.Task is null)
{
    throw new InvalidOperationException($"expected task response, got {response.PayloadCase}");
}

AgentTask task = response.Task;
TaskState initialState = task.Status.State;

while (!task.Status.State.IsTerminal())
{
    await Task.Delay(500, cts.Token);
    task = await client.GetTaskAsync(new GetTaskRequest { Id = task.Id }, cts.Token);
}

var payload = new
{
    cardName = card.Name,
    cardUrl = card.SupportedInterfaces[0].Url,
    protocolBinding = card.SupportedInterfaces[0].ProtocolBinding,
    protocolVersion = card.SupportedInterfaces[0].ProtocolVersion,
    taskId = task.Id,
    initialState,
    finalState = task.Status.State,
    message = task.Status.Message?.Parts?.FirstOrDefault()?.Text,
};

Console.WriteLine(JsonSerializer.Serialize(payload));
