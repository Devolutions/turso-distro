using System.Text.Json.Serialization;

namespace Turso;

[JsonSourceGenerationOptions(DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull)]
[JsonSerializable(typeof(TursoRemoteClient.RemotePipelineRequest))]
[JsonSerializable(typeof(RemotePipelineResponse))]
[JsonSerializable(typeof(RemoteStatementResult))]
[JsonSerializable(typeof(RemoteBatchResult))]
// RemoteRequestValue.Value is typed object; register the runtime types stored in it.
[JsonSerializable(typeof(string))]
[JsonSerializable(typeof(double))]
[JsonSerializable(typeof(long))]
internal sealed partial class TursoRemoteJsonContext : JsonSerializerContext;
