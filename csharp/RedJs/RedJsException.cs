namespace RedJs;

/// <summary>A JavaScript exception surfaced to the host, with the JS stack
/// captured at throw time (the exception value itself is not retained).</summary>
public sealed class RedJsException : Exception
{
    public string JsStack { get; }

    public RedJsException(string message, string jsStack)
        : base(string.IsNullOrEmpty(jsStack) ? message : $"{message}\n{jsStack}")
    {
        JsStack = jsStack;
    }
}

/// <summary>Non-exception engine failures (bad argument, pool exhaustion).</summary>
public sealed class RedJsEngineException : Exception
{
    public int Status { get; }

    public RedJsEngineException(string operation, int status)
        : base($"redjs: {operation} failed with status {status}")
    {
        Status = status;
    }
}
