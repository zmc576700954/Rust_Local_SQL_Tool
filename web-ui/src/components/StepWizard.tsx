import React, { useState } from 'react';
import { ArrowRight, ArrowLeft, Check, AlertTriangle, X, Loader2 } from 'lucide-react';

export interface WizardStep {
  id: string;
  title: string;
  content: React.ReactNode;
  isValid?: boolean; // If false, Next/Finish is disabled
}

interface StepWizardProps {
  steps: WizardStep[];
  onFinish: () => Promise<void> | void;
  onCancel?: () => void;
  finalWarningMessage?: string;
  title?: string;
  isLoading?: boolean;
}

export function StepWizard({
  steps,
  onFinish,
  onCancel,
  finalWarningMessage = 'Are you sure you want to proceed? This action may modify your database.',
  title,
  isLoading = false
}: StepWizardProps) {
  const [currentStepIndex, setCurrentStepIndex] = useState(0);
  const [isFinishing, setIsFinishing] = useState(false);

  const currentStep = steps[currentStepIndex];
  const isFirstStep = currentStepIndex === 0;
  const isLastStep = currentStepIndex === steps.length - 1;

  const handleNext = () => {
    if (!isLastStep && (currentStep.isValid !== false)) {
      setCurrentStepIndex(prev => prev + 1);
    }
  };

  const handlePrev = () => {
    if (!isFirstStep) {
      setCurrentStepIndex(prev => prev - 1);
    }
  };

  const handleFinish = async () => {
    if (currentStep.isValid === false) return;
    
    setIsFinishing(true);
    try {
      await onFinish();
    } finally {
      setIsFinishing(false);
    }
  };

  return (
    <div className="flex flex-col h-full bg-[#161b22] text-gray-300 rounded-xl overflow-hidden shadow-2xl border border-[#30363d]">
      {/* Header */}
      {title && (
        <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117] shrink-0">
          <h3 className="text-gray-200 font-bold text-lg">{title}</h3>
          {onCancel && (
            <button onClick={onCancel} className="text-gray-500 hover:text-white transition-colors">
              <X className="w-5 h-5" />
            </button>
          )}
        </div>
      )}

      {/* Step Indicators */}
      <div className="px-6 py-4 border-b border-[#30363d] bg-[#0d1117]">
        <div className="flex items-center justify-between max-w-3xl mx-auto">
          {steps.map((step, index) => (
            <React.Fragment key={step.id}>
              <div className={`flex flex-col items-center gap-2 ${index <= currentStepIndex ? 'text-blue-400' : 'text-gray-500'}`}>
                <div className={`w-8 h-8 rounded-full flex items-center justify-center border-2 ${
                  index < currentStepIndex 
                    ? 'bg-blue-500/20 border-blue-500 text-blue-400' 
                    : index === currentStepIndex
                      ? 'border-blue-400 text-blue-400'
                      : 'border-gray-600 text-gray-500'
                }`}>
                  {index < currentStepIndex ? <Check className="w-4 h-4" /> : index + 1}
                </div>
                <span className="text-xs font-medium">{step.title}</span>
              </div>
              {index < steps.length - 1 && (
                <div className={`flex-1 h-[2px] mx-4 ${index < currentStepIndex ? 'bg-blue-500/50' : 'bg-gray-700'}`} />
              )}
            </React.Fragment>
          ))}
        </div>
      </div>

      {/* Content */}
      <div className="flex-1 p-6 overflow-y-auto min-h-[300px]">
        {isLastStep && finalWarningMessage && (
          <div className="mb-6 bg-yellow-500/10 border border-yellow-500/50 text-yellow-200 p-4 rounded-lg flex items-start gap-3">
            <AlertTriangle className="w-5 h-5 mt-0.5 shrink-0 text-yellow-500" />
            <div>
              <h4 className="font-bold mb-1">Safety Warning</h4>
              <p className="text-sm opacity-90">{finalWarningMessage}</p>
              <p className="text-xs mt-2 opacity-75 italic">Note: This is a deterministic operation and does not involve AI.</p>
            </div>
          </div>
        )}
        <div className="h-full">
          {currentStep.content}
        </div>
      </div>

      {/* Footer / Actions */}
      <div className="px-6 py-4 border-t border-[#30363d] bg-[#0d1117] flex items-center justify-between shrink-0">
        <div>
          {!isFirstStep && (
            <button
              onClick={handlePrev}
              disabled={isFinishing || isLoading}
              className="px-4 py-2 rounded border border-[#30363d] hover:bg-[#30363d] text-gray-300 flex items-center gap-2 transition-colors disabled:opacity-50"
            >
              <ArrowLeft className="w-4 h-4" />
              Previous
            </button>
          )}
        </div>
        
        <div className="flex items-center gap-3">
          {onCancel && (
            <button
              onClick={onCancel}
              disabled={isFinishing || isLoading}
              className="px-4 py-2 rounded text-gray-400 hover:text-white transition-colors disabled:opacity-50"
            >
              Cancel
            </button>
          )}
          
          {isLastStep ? (
            <button
              onClick={handleFinish}
              disabled={currentStep.isValid === false || isFinishing || isLoading}
              className="px-6 py-2 rounded bg-blue-600 hover:bg-blue-500 text-white font-medium flex items-center gap-2 transition-colors disabled:opacity-50 shadow-lg shadow-blue-900/20"
            >
              {isFinishing || isLoading ? (
                <>
                  <Loader2 className="w-4 h-4 animate-spin" />
                  <span>{isFinishing ? 'Executing...' : 'Loading...'}</span>
                </>
              ) : (
                <>
                  Execute <Check className="w-4 h-4" />
                </>
              )}
            </button>
          ) : (
            <button
              onClick={handleNext}
              disabled={currentStep.isValid === false || isLoading}
              className="px-6 py-2 rounded bg-blue-600 hover:bg-blue-500 text-white font-medium flex items-center gap-2 transition-colors disabled:opacity-50 shadow-lg shadow-blue-900/20"
            >
              {isLoading ? (
                <>
                  <Loader2 className="w-4 h-4 animate-spin" />
                  <span>Loading...</span>
                </>
              ) : (
                <>
                  Next <ArrowRight className="w-4 h-4" />
                </>
              )}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
