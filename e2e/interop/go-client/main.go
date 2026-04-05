package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/a2aproject/a2a-go/v2/a2a"
	"github.com/a2aproject/a2a-go/v2/a2aclient"
	"github.com/a2aproject/a2a-go/v2/a2aclient/agentcard"
)

type interopResult struct {
	CardName        string        `json:"cardName"`
	CardURL         string        `json:"cardUrl"`
	ProtocolBinding string        `json:"protocolBinding"`
	ProtocolVersion string        `json:"protocolVersion"`
	TaskID          a2a.TaskID    `json:"taskId"`
	InitialState    a2a.TaskState `json:"initialState"`
	FinalState      a2a.TaskState `json:"finalState"`
	Message         string        `json:"message"`
}

func main() {
	baseURL := "http://127.0.0.1:38080"
	if len(os.Args) > 1 {
		baseURL = os.Args[1]
	}

	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	card, err := agentcard.DefaultResolver.Resolve(ctx, baseURL)
	if err != nil {
		panic(err)
	}
	if len(card.SupportedInterfaces) == 0 {
		panic("agent card did not expose a supported interface")
	}

	client, err := a2aclient.NewFromCard(ctx, card)
	if err != nil {
		panic(err)
	}

	taskID := a2a.TaskID(fmt.Sprintf("go-interop-%d", time.Now().UnixNano()))
	message := &a2a.Message{
		ID:        fmt.Sprintf("msg-%s", taskID),
		Role:      a2a.MessageRoleUser,
		TaskID:    taskID,
		ContextID: string(taskID),
		Parts:     []*a2a.Part{a2a.NewTextPart("hello from go")},
	}

	result, err := client.SendMessage(ctx, &a2a.SendMessageRequest{
		Message: message,
		Config: &a2a.SendMessageConfig{
			ReturnImmediately: true,
		},
	})
	if err != nil {
		panic(err)
	}

	task, ok := result.(*a2a.Task)
	if !ok {
		panic(fmt.Sprintf("expected task response, got %T", result))
	}

	initialState := task.Status.State
	for !task.Status.State.Terminal() {
		time.Sleep(500 * time.Millisecond)
		task, err = client.GetTask(ctx, &a2a.GetTaskRequest{ID: task.ID})
		if err != nil {
			panic(err)
		}
	}

	finalMessage := ""
	if task.Status.Message != nil && len(task.Status.Message.Parts) > 0 {
		finalMessage = task.Status.Message.Parts[0].Text()
	}

	if err := json.NewEncoder(os.Stdout).Encode(interopResult{
		CardName:        card.Name,
		CardURL:         card.SupportedInterfaces[0].URL,
		ProtocolBinding: string(card.SupportedInterfaces[0].ProtocolBinding),
		ProtocolVersion: string(card.SupportedInterfaces[0].ProtocolVersion),
		TaskID:          task.ID,
		InitialState:    initialState,
		FinalState:      task.Status.State,
		Message:         finalMessage,
	}); err != nil {
		panic(err)
	}
}
