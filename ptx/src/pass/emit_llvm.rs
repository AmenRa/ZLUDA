// We use Raw LLVM-C bindings here because using inkwell is just not worth it.
// Specifically the issue is with builder functions. We maintain the mapping
// between ZLUDA identifiers and LLVM values. When using inkwell, LLVM values
// are kept as instances `AnyValueEnum`. Now look at the signature of
// `Builder::build_int_add(...)`:
//   pub fn build_int_add<T: IntMathValue<'ctx>>(&self, lhs: T, rhs: T, name: &str, ) -> Result<T, BuilderError>
// At this point both lhs and rhs are `AnyValueEnum`. To call
// `build_int_add(...)` we would have to do something like this:
//   if let (Ok(lhs), Ok(rhs)) = (lhs.as_int(), rhs.as_int()) {
//       builder.build_int_add(lhs, rhs, dst)?;
//   } else if let (Ok(lhs), Ok(rhs)) = (lhs.as_pointer(), rhs.as_pointer()) {
//      builder.build_int_add(lhs, rhs, dst)?;
//   } else if let (Ok(lhs), Ok(rhs)) = (lhs.as_vector(), rhs.as_vector()) {
//       builder.build_int_add(lhs, rhs, dst)?;
//   } else {
//       return Err(error_unrachable());
//   }
// while with plain LLVM-C it's just:
//   unsafe { LLVMBuildAdd(builder, lhs, rhs, dst) };

use std::convert::{TryFrom, TryInto};
use std::ffi::CStr;
use std::ops::Deref;
use std::ptr;

use super::*;
use llvm_zluda::analysis::{LLVMVerifierFailureAction, LLVMVerifyModule};
use llvm_zluda::bit_writer::LLVMWriteBitcodeToMemoryBuffer;
use llvm_zluda::core::*;
use llvm_zluda::prelude::*;
use llvm_zluda::{LLVMCallConv, LLVMZludaBuildAlloca};

const LLVM_UNNAMED: &CStr = c"";
// https://llvm.org/docs/AMDGPUUsage.html#address-spaces
const GENERIC_ADDRESS_SPACE: u32 = 0;
const GLOBAL_ADDRESS_SPACE: u32 = 1;
const SHARED_ADDRESS_SPACE: u32 = 3;
const CONSTANT_ADDRESS_SPACE: u32 = 4;
const PRIVATE_ADDRESS_SPACE: u32 = 5;

struct Context(LLVMContextRef);

impl Context {
    fn new() -> Self {
        Self(unsafe { LLVMContextCreate() })
    }

    fn get(&self) -> LLVMContextRef {
        self.0
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            LLVMContextDispose(self.0);
        }
    }
}

struct Module(LLVMModuleRef);

impl Module {
    fn new(ctx: &Context, name: &CStr) -> Self {
        Self(unsafe { LLVMModuleCreateWithNameInContext(name.as_ptr(), ctx.get()) })
    }

    fn get(&self) -> LLVMModuleRef {
        self.0
    }

    fn verify(&self) -> Result<(), Message> {
        let mut err = ptr::null_mut();
        let error = unsafe {
            LLVMVerifyModule(
                self.get(),
                LLVMVerifierFailureAction::LLVMReturnStatusAction,
                &mut err,
            )
        };
        if error == 1 && err != ptr::null_mut() {
            Err(Message(unsafe { CStr::from_ptr(err) }))
        } else {
            Ok(())
        }
    }

    fn write_bitcode_to_memory(&self) -> MemoryBuffer {
        let memory_buffer = unsafe { LLVMWriteBitcodeToMemoryBuffer(self.get()) };
        MemoryBuffer(memory_buffer)
    }

    fn write_to_stderr(&self) {
        unsafe { LLVMDumpModule(self.get()) };
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        unsafe {
            LLVMDisposeModule(self.0);
        }
    }
}

struct Builder(LLVMBuilderRef);

impl Builder {
    fn new(ctx: &Context) -> Self {
        Self::new_raw(ctx.get())
    }

    fn new_raw(ctx: LLVMContextRef) -> Self {
        Self(unsafe { LLVMCreateBuilderInContext(ctx) })
    }

    fn get(&self) -> LLVMBuilderRef {
        self.0
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        unsafe {
            LLVMDisposeBuilder(self.0);
        }
    }
}

struct Message(&'static CStr);

impl Drop for Message {
    fn drop(&mut self) {
        unsafe {
            LLVMDisposeMessage(self.0.as_ptr().cast_mut());
        }
    }
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

pub struct MemoryBuffer(LLVMMemoryBufferRef);

impl Drop for MemoryBuffer {
    fn drop(&mut self) {
        unsafe {
            LLVMDisposeMemoryBuffer(self.0);
        }
    }
}

impl Deref for MemoryBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        let data = unsafe { LLVMGetBufferStart(self.0) };
        let len = unsafe { LLVMGetBufferSize(self.0) };
        unsafe { std::slice::from_raw_parts(data.cast(), len) }
    }
}

pub(super) fn run<'input>(
    id_defs: GlobalStringIdentResolver2<'input>,
    directives: Vec<Directive2<'input, ast::Instruction<SpirvWord>, SpirvWord>>,
) -> Result<MemoryBuffer, TranslateError> {
    let context = Context::new();
    let module = Module::new(&context, LLVM_UNNAMED);
    let mut emit_ctx = ModuleEmitContext::new(&context, &module, &id_defs);
    for directive in directives {
        match directive {
            Directive2::Variable(..) => todo!(),
            Directive2::Method(method) => emit_ctx.emit_method(method)?,
        }
    }
    module.write_to_stderr();
    if let Err(err) = module.verify() {
        panic!("{:?}", err);
    }
    Ok(module.write_bitcode_to_memory())
}

struct ModuleEmitContext<'a, 'input> {
    context: LLVMContextRef,
    module: LLVMModuleRef,
    builder: Builder,
    id_defs: &'a GlobalStringIdentResolver2<'input>,
    resolver: ResolveIdent,
}

impl<'a, 'input> ModuleEmitContext<'a, 'input> {
    fn new(
        context: &Context,
        module: &Module,
        id_defs: &'a GlobalStringIdentResolver2<'input>,
    ) -> Self {
        ModuleEmitContext {
            context: context.get(),
            module: module.get(),
            builder: Builder::new(context),
            id_defs,
            resolver: ResolveIdent::new(&id_defs),
        }
    }

    fn kernel_call_convention() -> u32 {
        LLVMCallConv::LLVMAMDGPUKERNELCallConv as u32
    }

    fn func_call_convention() -> u32 {
        LLVMCallConv::LLVMCCallConv as u32
    }

    fn emit_method(
        &mut self,
        method: Function2<'input, ast::Instruction<SpirvWord>, SpirvWord>,
    ) -> Result<(), TranslateError> {
        let func_decl = method.func_decl;
        let name = method
            .import_as
            .as_deref()
            .or_else(|| match func_decl.name {
                ast::MethodName::Kernel(name) => Some(name),
                ast::MethodName::Func(id) => self.id_defs.ident_map[&id].name.as_deref(),
            })
            .ok_or_else(|| error_unreachable())?;
        let name = CString::new(name).map_err(|_| error_unreachable())?;
        let fn_type = get_function_type(
            self.context,
            func_decl.return_arguments.iter().map(|v| &v.v_type),
            func_decl
                .input_arguments
                .iter()
                .map(|v| get_input_argument_type(self.context, &v.v_type, v.state_space)),
        )?;
        let fn_ = unsafe { LLVMAddFunction(self.module, name.as_ptr(), fn_type) };
        if let ast::MethodName::Func(name) = func_decl.name {
            self.resolver.register(name, fn_);
        }
        for (i, param) in func_decl.input_arguments.iter().enumerate() {
            let value = unsafe { LLVMGetParam(fn_, i as u32) };
            let name = self.resolver.get_or_add(param.name);
            unsafe { LLVMSetValueName2(value, name.as_ptr().cast(), name.len()) };
            self.resolver.register(param.name, value);
            if func_decl.name.is_kernel() {
                let attr_kind = unsafe {
                    LLVMGetEnumAttributeKindForName(b"byref".as_ptr().cast(), b"byref".len())
                };
                let attr = unsafe {
                    LLVMCreateTypeAttribute(
                        self.context,
                        attr_kind,
                        get_type(self.context, &param.v_type)?,
                    )
                };
                unsafe { LLVMAddAttributeAtIndex(fn_, i as u32 + 1, attr) };
            }
        }
        let call_conv = if func_decl.name.is_kernel() {
            Self::kernel_call_convention()
        } else {
            Self::func_call_convention()
        };
        unsafe { LLVMSetFunctionCallConv(fn_, call_conv) };
        if let Some(statements) = method.body {
            let variables_bb =
                unsafe { LLVMAppendBasicBlockInContext(self.context, fn_, LLVM_UNNAMED.as_ptr()) };
            let variables_builder = Builder::new_raw(self.context);
            unsafe { LLVMPositionBuilderAtEnd(variables_builder.get(), variables_bb) };
            let real_bb =
                unsafe { LLVMAppendBasicBlockInContext(self.context, fn_, LLVM_UNNAMED.as_ptr()) };
            unsafe { LLVMPositionBuilderAtEnd(self.builder.get(), real_bb) };
            let mut method_emitter = MethodEmitContext::new(self, fn_, variables_builder);
            for statement in statements {
                method_emitter.emit_statement(statement)?;
            }
            unsafe { LLVMBuildBr(method_emitter.variables_builder.get(), real_bb) };
        }
        Ok(())
    }
}

fn get_input_argument_type(
    context: LLVMContextRef,
    v_type: &ptx_parser::Type,
    state_space: ptx_parser::StateSpace,
) -> Result<LLVMTypeRef, TranslateError> {
    match state_space {
        ptx_parser::StateSpace::ParamEntry => {
            Ok(unsafe { LLVMPointerTypeInContext(context, get_state_space(state_space)?) })
        }
        ptx_parser::StateSpace::Reg => get_type(context, v_type),
        _ => return Err(error_unreachable()),
    }
}

struct MethodEmitContext<'a, 'input> {
    context: LLVMContextRef,
    module: LLVMModuleRef,
    method: LLVMValueRef,
    builder: LLVMBuilderRef,
    id_defs: &'a GlobalStringIdentResolver2<'input>,
    variables_builder: Builder,
    resolver: &'a mut ResolveIdent,
}

impl<'a, 'input> MethodEmitContext<'a, 'input> {
    fn new<'x>(
        parent: &'a mut ModuleEmitContext<'x, 'input>,
        method: LLVMValueRef,
        variables_builder: Builder,
    ) -> MethodEmitContext<'a, 'input> {
        MethodEmitContext {
            context: parent.context,
            module: parent.module,
            builder: parent.builder.get(),
            id_defs: parent.id_defs,
            variables_builder,
            resolver: &mut parent.resolver,
            method,
        }
    }

    fn emit_statement(
        &mut self,
        statement: Statement<ast::Instruction<SpirvWord>, SpirvWord>,
    ) -> Result<(), TranslateError> {
        Ok(match statement {
            Statement::Variable(var) => self.emit_variable(var)?,
            Statement::Label(label) => self.emit_label(label),
            Statement::Instruction(inst) => self.emit_instruction(inst)?,
            Statement::Conditional(_) => todo!(),
            Statement::LoadVar(var) => self.emit_load_variable(var)?,
            Statement::StoreVar(store) => self.emit_store_var(store)?,
            Statement::Conversion(conversion) => self.emit_conversion(conversion)?,
            Statement::Constant(constant) => self.emit_constant(constant)?,
            Statement::RetValue(_, _) => todo!(),
            Statement::PtrAccess(_) => todo!(),
            Statement::RepackVector(_) => todo!(),
            Statement::FunctionPointer(_) => todo!(),
            Statement::VectorAccess(_) => todo!(),
        })
    }

    fn emit_variable(&mut self, var: ast::Variable<SpirvWord>) -> Result<(), TranslateError> {
        let alloca = unsafe {
            LLVMZludaBuildAlloca(
                self.variables_builder.get(),
                get_type(self.context, &var.v_type)?,
                get_state_space(var.state_space)?,
                self.resolver.get_or_add_raw(var.name),
            )
        };
        self.resolver.register(var.name, alloca);
        if let Some(align) = var.align {
            unsafe { LLVMSetAlignment(alloca, align) };
        }
        if !var.array_init.is_empty() {
            todo!()
        }
        Ok(())
    }

    fn emit_label(&mut self, label: SpirvWord) {
        let block = unsafe {
            LLVMAppendBasicBlockInContext(
                self.context,
                self.method,
                self.resolver.get_or_add_raw(label),
            )
        };
        let last_block = unsafe { LLVMGetInsertBlock(self.builder) };
        if unsafe { LLVMGetBasicBlockTerminator(last_block) } == ptr::null_mut() {
            unsafe { LLVMBuildBr(self.builder, block) };
        }
        unsafe { LLVMPositionBuilderAtEnd(self.builder, block) };
    }

    fn emit_store_var(&mut self, store: StoreVarDetails) -> Result<(), TranslateError> {
        let ptr = self.resolver.value(store.arg.src1)?;
        let value = self.resolver.value(store.arg.src2)?;
        unsafe { LLVMBuildStore(self.builder, value, ptr) };
        Ok(())
    }

    fn emit_instruction(
        &mut self,
        inst: ast::Instruction<SpirvWord>,
    ) -> Result<(), TranslateError> {
        match inst {
            ast::Instruction::Mov { data, arguments } => self.emit_mov(data, arguments),
            ast::Instruction::Ld { data, arguments } => self.emit_ld(data, arguments),
            ast::Instruction::Add { data, arguments } => self.emit_add(data, arguments),
            ast::Instruction::St { data, arguments } => self.emit_st(data, arguments),
            ast::Instruction::Mul { data, arguments } => todo!(),
            ast::Instruction::Setp { data, arguments } => todo!(),
            ast::Instruction::SetpBool { data, arguments } => todo!(),
            ast::Instruction::Not { data, arguments } => todo!(),
            ast::Instruction::Or { data, arguments } => todo!(),
            ast::Instruction::And { data, arguments } => todo!(),
            ast::Instruction::Bra { arguments } => todo!(),
            ast::Instruction::Call { data, arguments } => self.emit_call(data, arguments),
            ast::Instruction::Cvt { data, arguments } => todo!(),
            ast::Instruction::Shr { data, arguments } => todo!(),
            ast::Instruction::Shl { data, arguments } => todo!(),
            ast::Instruction::Ret { data } => Ok(self.emit_ret(data)),
            ast::Instruction::Cvta { data, arguments } => todo!(),
            ast::Instruction::Abs { data, arguments } => todo!(),
            ast::Instruction::Mad { data, arguments } => todo!(),
            ast::Instruction::Fma { data, arguments } => todo!(),
            ast::Instruction::Sub { data, arguments } => todo!(),
            ast::Instruction::Min { data, arguments } => todo!(),
            ast::Instruction::Max { data, arguments } => todo!(),
            ast::Instruction::Rcp { data, arguments } => todo!(),
            ast::Instruction::Sqrt { data, arguments } => todo!(),
            ast::Instruction::Rsqrt { data, arguments } => todo!(),
            ast::Instruction::Selp { data, arguments } => todo!(),
            ast::Instruction::Bar { data, arguments } => todo!(),
            ast::Instruction::Atom { data, arguments } => todo!(),
            ast::Instruction::AtomCas { data, arguments } => todo!(),
            ast::Instruction::Div { data, arguments } => todo!(),
            ast::Instruction::Neg { data, arguments } => todo!(),
            ast::Instruction::Sin { data, arguments } => todo!(),
            ast::Instruction::Cos { data, arguments } => todo!(),
            ast::Instruction::Lg2 { data, arguments } => todo!(),
            ast::Instruction::Ex2 { data, arguments } => todo!(),
            ast::Instruction::Clz { data, arguments } => todo!(),
            ast::Instruction::Brev { data, arguments } => todo!(),
            ast::Instruction::Popc { data, arguments } => todo!(),
            ast::Instruction::Xor { data, arguments } => todo!(),
            ast::Instruction::Rem { data, arguments } => todo!(),
            ast::Instruction::Bfe { data, arguments } => todo!(),
            ast::Instruction::Bfi { data, arguments } => todo!(),
            ast::Instruction::PrmtSlow { arguments } => todo!(),
            ast::Instruction::Prmt { data, arguments } => todo!(),
            ast::Instruction::Activemask { arguments } => todo!(),
            ast::Instruction::Membar { data } => todo!(),
            ast::Instruction::Trap {} => todo!(),
        }
    }

    fn emit_ld(
        &mut self,
        data: ast::LdDetails,
        arguments: ast::LdArgs<SpirvWord>,
    ) -> Result<(), TranslateError> {
        if data.non_coherent {
            todo!()
        }
        if data.qualifier != ast::LdStQualifier::Weak {
            todo!()
        }
        let builder = self.builder;
        let type_ = get_type(self.context, &data.typ)?;
        let ptr = self.resolver.value(arguments.src)?;
        self.resolver.with_result(arguments.dst, |dst| unsafe {
            LLVMBuildLoad2(builder, type_, ptr, dst)
        });
        Ok(())
    }

    fn emit_load_variable(&mut self, var: LoadVarDetails) -> Result<(), TranslateError> {
        if var.member_index.is_some() {
            todo!()
        }
        let builder = self.builder;
        let type_ = get_type(self.context, &var.typ)?;
        let ptr = self.resolver.value(var.arg.src)?;
        self.resolver.with_result(var.arg.dst, |dst| unsafe {
            LLVMBuildLoad2(builder, type_, ptr, dst)
        });
        Ok(())
    }

    fn emit_conversion(&mut self, conversion: ImplicitConversion) -> Result<(), TranslateError> {
        let builder = self.builder;
        match conversion.kind {
            ConversionKind::Default => todo!(),
            ConversionKind::SignExtend => todo!(),
            ConversionKind::BitToPtr => {
                let src = self.resolver.value(conversion.src)?;
                let type_ = get_pointer_type(self.context, conversion.to_space)?;
                self.resolver.with_result(conversion.dst, |dst| unsafe {
                    LLVMBuildIntToPtr(builder, src, type_, dst)
                });
                Ok(())
            }
            ConversionKind::PtrToPtr => todo!(),
            ConversionKind::AddressOf => todo!(),
        }
    }

    fn emit_constant(&mut self, constant: ConstantDefinition) -> Result<(), TranslateError> {
        let type_ = get_scalar_type(self.context, constant.typ);
        let value = match constant.value {
            ast::ImmediateValue::U64(x) => unsafe { LLVMConstInt(type_, x, 0) },
            ast::ImmediateValue::S64(x) => unsafe { LLVMConstInt(type_, x as u64, 0) },
            ast::ImmediateValue::F32(x) => unsafe { LLVMConstReal(type_, x as f64) },
            ast::ImmediateValue::F64(x) => unsafe { LLVMConstReal(type_, x) },
        };
        self.resolver.register(constant.dst, value);
        Ok(())
    }

    fn emit_add(
        &mut self,
        data: ast::ArithDetails,
        arguments: ast::AddArgs<SpirvWord>,
    ) -> Result<(), TranslateError> {
        let builder = self.builder;
        let src1 = self.resolver.value(arguments.src1)?;
        let src2 = self.resolver.value(arguments.src2)?;
        let fn_ = match data {
            ast::ArithDetails::Integer(integer) => LLVMBuildAdd,
            ast::ArithDetails::Float(float) => LLVMBuildFAdd,
        };
        self.resolver.with_result(arguments.dst, |dst| unsafe {
            fn_(builder, src1, src2, dst)
        });
        Ok(())
    }

    fn emit_st(
        &self,
        data: ptx_parser::StData,
        arguments: ptx_parser::StArgs<SpirvWord>,
    ) -> Result<(), TranslateError> {
        let ptr = self.resolver.value(arguments.src1)?;
        let value = self.resolver.value(arguments.src2)?;
        if data.qualifier != ast::LdStQualifier::Weak {
            todo!()
        }
        unsafe { LLVMBuildStore(self.builder, value, ptr) };
        Ok(())
    }

    fn emit_ret(&self, _data: ptx_parser::RetData) {
        unsafe { LLVMBuildRetVoid(self.builder) };
    }

    fn emit_call(
        &mut self,
        data: ptx_parser::CallDetails,
        arguments: ptx_parser::CallArgs<SpirvWord>,
    ) -> Result<(), TranslateError> {
        if cfg!(debug_assertions) {
            for (_, space) in data.return_arguments.iter() {
                if *space != ast::StateSpace::Reg {
                    panic!()
                }
            }
            for (_, space) in data.input_arguments.iter() {
                if *space != ast::StateSpace::Reg {
                    panic!()
                }
            }
        }
        let name = match (&*data.return_arguments, &*arguments.return_arguments) {
            ([], []) => LLVM_UNNAMED.as_ptr(),
            ([(type_, _)], [dst]) => self.resolver.get_or_add_raw(*dst),
            _ => todo!(),
        };
        let type_ = get_function_type(
            self.context,
            data.return_arguments.iter().map(|(type_, space)| type_),
            data.input_arguments
                .iter()
                .map(|(type_, space)| get_input_argument_type(self.context, &type_, *space)),
        )?;
        let mut input_arguments = arguments
            .input_arguments
            .iter()
            .map(|arg| self.resolver.value(*arg))
            .collect::<Result<Vec<_>, _>>()?;
        let llvm_fn = unsafe {
            LLVMBuildCall2(
                self.builder,
                type_,
                self.resolver.value(arguments.func)?,
                input_arguments.as_mut_ptr(),
                input_arguments.len() as u32,
                name,
            )
        };
        match &*arguments.return_arguments {
            [] => {}
            [name] => {
                self.resolver.register(*name, llvm_fn);
            }
            _ => todo!(),
        }
        Ok(())
    }

    fn emit_mov(
        &mut self,
        _data: ptx_parser::MovDetails,
        arguments: ptx_parser::MovArgs<SpirvWord>,
    ) -> Result<(), TranslateError> {
        self.resolver
            .register(arguments.dst, self.resolver.value(arguments.src)?);
        Ok(())
    }
}

fn get_pointer_type<'ctx>(
    context: LLVMContextRef,
    to_space: ast::StateSpace,
) -> Result<LLVMTypeRef, TranslateError> {
    Ok(unsafe { LLVMPointerTypeInContext(context, get_state_space(to_space)?) })
}

fn get_type(context: LLVMContextRef, type_: &ast::Type) -> Result<LLVMTypeRef, TranslateError> {
    Ok(match type_ {
        ast::Type::Scalar(scalar) => get_scalar_type(context, *scalar),
        ast::Type::Vector(size, scalar) => {
            let base_type = get_scalar_type(context, *scalar);
            unsafe { LLVMVectorType(base_type, *size as u32) }
        }
        ast::Type::Array(vec, scalar, dimensions) => {
            let mut underlying_type = get_scalar_type(context, *scalar);
            if let Some(size) = vec {
                underlying_type = unsafe { LLVMVectorType(underlying_type, size.get() as u32) };
            }
            if dimensions.is_empty() {
                return Ok(unsafe { LLVMArrayType2(underlying_type, 0) });
            }
            dimensions
                .iter()
                .rfold(underlying_type, |result, dimension| unsafe {
                    LLVMArrayType2(result, *dimension as u64)
                })
        }
        ast::Type::Pointer(_, space) => get_pointer_type(context, *space)?,
    })
}

fn get_scalar_type(context: LLVMContextRef, type_: ast::ScalarType) -> LLVMTypeRef {
    match type_ {
        ast::ScalarType::Pred => unsafe { LLVMInt1TypeInContext(context) },
        ast::ScalarType::S8 | ast::ScalarType::B8 | ast::ScalarType::U8 => unsafe {
            LLVMInt8TypeInContext(context)
        },
        ast::ScalarType::B16 | ast::ScalarType::U16 | ast::ScalarType::S16 => unsafe {
            LLVMInt16TypeInContext(context)
        },
        ast::ScalarType::S32 | ast::ScalarType::B32 | ast::ScalarType::U32 => unsafe {
            LLVMInt32TypeInContext(context)
        },
        ast::ScalarType::U64 | ast::ScalarType::S64 | ast::ScalarType::B64 => unsafe {
            LLVMInt64TypeInContext(context)
        },
        ast::ScalarType::B128 => unsafe { LLVMInt128TypeInContext(context) },
        ast::ScalarType::F16 => unsafe { LLVMHalfTypeInContext(context) },
        ast::ScalarType::F32 => unsafe { LLVMFloatTypeInContext(context) },
        ast::ScalarType::F64 => unsafe { LLVMDoubleTypeInContext(context) },
        ast::ScalarType::BF16 => unsafe { LLVMBFloatTypeInContext(context) },
        ast::ScalarType::U16x2 => todo!(),
        ast::ScalarType::S16x2 => todo!(),
        ast::ScalarType::F16x2 => todo!(),
        ast::ScalarType::BF16x2 => todo!(),
    }
}

fn get_function_type<'a>(
    context: LLVMContextRef,
    mut return_args: impl ExactSizeIterator<Item = &'a ast::Type>,
    input_args: impl ExactSizeIterator<Item = Result<LLVMTypeRef, TranslateError>>,
) -> Result<LLVMTypeRef, TranslateError> {
    let mut input_args: Vec<*mut llvm_zluda::LLVMType> =
        input_args.collect::<Result<Vec<_>, _>>()?;
    let return_type = match return_args.len() {
        0 => unsafe { LLVMVoidTypeInContext(context) },
        1 => get_type(context, return_args.next().unwrap())?,
        _ => todo!(),
    };
    Ok(unsafe {
        LLVMFunctionType(
            return_type,
            input_args.as_mut_ptr(),
            input_args.len() as u32,
            0,
        )
    })
}

fn get_state_space(space: ast::StateSpace) -> Result<u32, TranslateError> {
    match space {
        ast::StateSpace::Reg => Ok(PRIVATE_ADDRESS_SPACE),
        ast::StateSpace::Generic => Ok(GENERIC_ADDRESS_SPACE),
        ast::StateSpace::Param => Err(TranslateError::Todo),
        ast::StateSpace::ParamEntry => Ok(CONSTANT_ADDRESS_SPACE),
        ast::StateSpace::ParamFunc => Err(TranslateError::Todo),
        ast::StateSpace::Local => Ok(PRIVATE_ADDRESS_SPACE),
        ast::StateSpace::Global => Ok(GLOBAL_ADDRESS_SPACE),
        ast::StateSpace::Const => Ok(CONSTANT_ADDRESS_SPACE),
        ast::StateSpace::Shared => Ok(SHARED_ADDRESS_SPACE),
        ast::StateSpace::SharedCta => Err(TranslateError::Todo),
        ast::StateSpace::SharedCluster => Err(TranslateError::Todo),
    }
}

struct ResolveIdent {
    words: HashMap<SpirvWord, String>,
    values: HashMap<SpirvWord, LLVMValueRef>,
}

impl ResolveIdent {
    fn new<'input>(_id_defs: &GlobalStringIdentResolver2<'input>) -> Self {
        ResolveIdent {
            words: HashMap::new(),
            values: HashMap::new(),
        }
    }

    fn get_or_ad_impl<'a, T>(&'a mut self, word: SpirvWord, fn_: impl FnOnce(&'a str) -> T) -> T {
        let str = match self.words.entry(word) {
            hash_map::Entry::Occupied(entry) => entry.into_mut(),
            hash_map::Entry::Vacant(entry) => {
                let mut text = word.0.to_string();
                text.push('\0');
                entry.insert(text)
            }
        };
        fn_(&str[..str.len() - 1])
    }

    fn get_or_add(&mut self, word: SpirvWord) -> &str {
        self.get_or_ad_impl(word, |x| x)
    }

    fn get_or_add_raw(&mut self, word: SpirvWord) -> *const i8 {
        self.get_or_add(word).as_ptr().cast()
    }

    fn register(&mut self, word: SpirvWord, v: LLVMValueRef) {
        self.values.insert(word, v);
    }

    fn value(&self, word: SpirvWord) -> Result<LLVMValueRef, TranslateError> {
        self.values
            .get(&word)
            .copied()
            .ok_or_else(|| error_unreachable())
    }

    fn with_result(&mut self, word: SpirvWord, fn_: impl FnOnce(*const i8) -> LLVMValueRef) {
        let t = self.get_or_ad_impl(word, |dst| fn_(dst.as_ptr().cast()));
        self.register(word, t);
    }
}
