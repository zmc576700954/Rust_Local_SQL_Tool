import { Component } from 'react';
import type { ErrorInfo, ReactNode } from 'react';
import { AlertTriangle, RefreshCcw } from 'lucide-react';
import { sanitizeForLog } from '../utils'

interface Props {
  children: ReactNode;
}

interface State {
  hasError: boolean;
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  public state: State = {
    hasError: false,
    error: null,
  };

  public static getDerivedStateFromError(error: Error): State {
    return { hasError: true, error };
  }

  public componentDidCatch(error: Error, errorInfo: ErrorInfo) {
    console.error('Uncaught error:', sanitizeForLog(error), sanitizeForLog(errorInfo));
  }

  private handleReset = () => {
    this.setState({ hasError: false, error: null });
    window.location.reload();
  };

  public render() {
    if (this.state.hasError) {
      return (
        <div className="min-h-screen flex items-center justify-center bg-[#0d1117] text-gray-300 p-4">
          <div className="max-w-md w-full bg-[#161b22] border border-[#30363d] rounded-lg p-6 shadow-xl flex flex-col items-center text-center space-y-4">
            <div className="p-3 bg-red-950/30 rounded-full">
              <AlertTriangle className="w-10 h-10 text-red-500" />
            </div>
            
            <h1 className="text-xl font-semibold text-gray-100">页面出错了</h1>
            
            <p className="text-sm text-gray-400">
              很抱歉，应用在渲染时遇到了意外的错误。这可能是由不稳定的网络或异常数据引起的。
            </p>

            {this.state.error && (
              <div className="w-full mt-4 p-3 bg-black/40 border border-red-900/30 rounded text-left overflow-x-auto">
                <code className="text-xs text-red-400 break-all">
                  {this.state.error.toString()}
                </code>
              </div>
            )}

            <button
              onClick={this.handleReset}
              className="mt-6 flex items-center space-x-2 px-4 py-2 bg-[#238636] hover:bg-[#2ea043] text-white rounded-md transition-colors text-sm font-medium"
            >
              <RefreshCcw className="w-4 h-4" />
              <span>刷新页面重试</span>
            </button>
          </div>
        </div>
      );
    }

    return this.props.children;
  }
}
